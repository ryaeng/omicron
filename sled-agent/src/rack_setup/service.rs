// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Rack Setup Service (RSS) implementation
//!
//! RSS triggers the initialization of:
//! - Sled Agents (giving them underlay addresses)
//! - Trust Quorum (coordinating between Sled Agents)
//! - Services (such as internal DNS, CRDB, Nexus)
//! - DNS records for those services
//! - Handoff to Nexus, for control of Control Plane management
//!
//! # Phases and Configuration Files
//!
//! Rack setup occurs in distinct phases which are denoted by the prescence of
//! configuration files.
//!
//! - /var/oxide/rss-sled-plan.toml (Sled Plan)
//! - /var/oxide/rss-service-plan.toml (Service Plan)
//! - /var/oxide/rss-plan-completed.marker (Plan Execution Complete)
//!
//! ## Sled Plan
//!
//! RSS should start as a service executing on a Sidecar-attached Gimlet
//! (Scrimlet). It must communicate with other sleds on the bootstrap network to
//! discover neighbors. RSS uses the bootstrap network to identify peers, assign
//! them subnets and UUIDs, and initialize a trust quorum. Once RSS decides
//! these values it commits them to a local file as the "Sled Plan", before
//! sending requests.
//!
//! As a result, restarting RSS should result in retransmission of the same
//! values, as long as the same configuration file is used.
//!
//! ## Service Plan
//!
//! After the trust quorum is established and Sled Agents are executing across
//! the rack, RSS can make the call on "what services should run where",
//! ensuring the minimal set of services necessary to execute Nexus are
//! operational. Critically, these include:
//! - Internal DNS: Necessary so internal services can discover each other
//! - CockroachDB: Necessary for Nexus to operate
//! - Nexus itself
//!
//! Once the distribution of these services is decided (which sled should run
//! what service? On what zpools should CockroachDB be provisioned?) it is
//! committed to the Service Plan, and executed.
//!
//! ## Execution Complete
//!
//! Once the both the Sled and Service plans have finished execution, handoff of
//! control to Nexus can occur. <https://rfd.shared.oxide.computer/rfd/0278>
//! covers this in more detail, but in short, RSS creates a "marker" file after
//! completing execution, and unconditionally calls the "handoff to Nexus" API
//! thereafter.

use super::config::SetupServiceConfig as Config;
use crate::bootstrap::ddm_admin_client::{DdmAdminClient, DdmError};
use crate::bootstrap::params::SledAgentRequest;
use crate::bootstrap::rss_handle::BootstrapAgentHandle;
use crate::params::{
    DatasetEnsureBody, ServiceType, ServiceZoneRequest, ZoneType,
};
use crate::rack_setup::plan::service::{
    Plan as ServicePlan, PlanError as ServicePlanError,
};
use crate::rack_setup::plan::sled::{
    generate_rack_secret, Plan as SledPlan, PlanError as SledPlanError,
};
use dns_service_client::multiclient::{
    DnsError, Resolver as DnsResolver, Updater as DnsUpdater,
};
use internal_dns_names::{ServiceName, SRV};
use nexus_client::{
    types as NexusTypes, Client as NexusClient, Error as NexusError,
};
use omicron_common::address::{get_sled_address, NEXUS_INTERNAL_PORT};
use omicron_common::backoff::{
    retry_notify, retry_policy_internal_service_aggressive, BackoffError,
};
use serde::{Deserialize, Serialize};
use sled_agent_client::{
    types as SledAgentTypes, Client as SledAgentClient, Error as SledAgentError,
};
use slog::Logger;
use sprockets_host::Ed25519Certificate;
use std::collections::{HashMap, HashSet};
use std::iter;
use std::net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::path::PathBuf;
use thiserror::Error;
use tokio::sync::OnceCell;

// The minimum number of sleds to initialize the rack.
const MINIMUM_SLED_COUNT: usize = 1;

/// Describes errors which may occur while operating the setup service.
#[derive(Error, Debug)]
pub enum SetupServiceError {
    #[error("I/O error while {message}: {err}")]
    Io {
        message: String,
        #[source]
        err: std::io::Error,
    },

    #[error("Cannot create plan for sled services: {0}")]
    ServicePlan(#[from] ServicePlanError),

    #[error("Cannot create plan for sled setup: {0}")]
    SledPlan(#[from] SledPlanError),

    #[error("Bad configuration for setting up rack: {0}")]
    BadConfig(String),

    #[error("Error initializing sled via sled-agent: {0}")]
    SledInitialization(String),

    #[error("Error making HTTP request to Sled Agent: {0}")]
    SledApi(#[from] SledAgentError<SledAgentTypes::Error>),

    #[error("Error making HTTP request to Nexus: {0}")]
    NexusApi(#[from] NexusError<NexusTypes::Error>),

    #[error("Error contacting ddmd: {0}")]
    DdmError(#[from] DdmError),

    #[error("Failed to monitor for peers: {0}")]
    PeerMonitor(#[from] tokio::sync::broadcast::error::RecvError),

    #[error("Failed to construct an HTTP client: {0}")]
    HttpClient(reqwest::Error),

    #[error("Failed to access DNS servers: {0}")]
    Dns(#[from] DnsError),
}

// The workload / information allocated to a single sled.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
struct SledAllocation {
    initialization_request: SledAgentRequest,
}

/// The interface to the Rack Setup Service.
pub struct RackSetupService {
    handle: tokio::task::JoinHandle<Result<(), SetupServiceError>>,
}

impl RackSetupService {
    /// Creates a new rack setup service, which runs in a background task.
    ///
    /// Arguments:
    /// - `log`: The logger.
    /// - `config`: The config file, which is used to setup the rack.
    /// - `peer_monitor`: The mechanism by which the setup service discovers
    ///   bootstrap agents on nearby sleds.
    /// - `local_bootstrap_agent`: Communication channel by which we can send
    ///   commands to our local bootstrap-agent (e.g., to initialize sled
    ///   agents).
    pub(crate) fn new(
        log: Logger,
        config: Config,
        local_bootstrap_agent: BootstrapAgentHandle,
        // TODO-cleanup: We should be collecting the device ID certs of all
        // trust quorum members over the management network. Currently we don't
        // have a management network, so we hard-code the list of members and
        // accept it as a parameter instead.
        member_device_id_certs: Vec<Ed25519Certificate>,
    ) -> Self {
        let handle = tokio::task::spawn(async move {
            let svc = ServiceInner::new(log.clone());
            if let Err(e) = svc
                .inject_rack_setup_requests(
                    &config,
                    local_bootstrap_agent,
                    &member_device_id_certs,
                )
                .await
            {
                warn!(log, "RSS injection failed: {}", e);
                Err(e)
            } else {
                Ok(())
            }
        });

        RackSetupService { handle }
    }

    /// Awaits the completion of the RSS service.
    pub async fn join(self) -> Result<(), SetupServiceError> {
        self.handle.await.expect("Rack Setup Service Task panicked")
    }
}

fn rss_completed_marker_path() -> PathBuf {
    std::path::Path::new(omicron_common::OMICRON_CONFIG_PATH)
        .join("rss-plan-completed.marker")
}

// Describes the options when awaiting for peers.
enum PeerExpectation {
    // Await a set of peers that matches this group of IPv6 addresses exactly.
    //
    // TODO: We currently don't deal with the case where:
    //
    // - RSS boots, sees some sleds, comes up with a plan.
    // - RSS reboots, sees a *different* set of sleds, and needs
    // to adjust the plan.
    //
    // This case is fairly tricky because some sleds may have
    // already received requests to initialize - modifying the
    // allocated subnets would be non-trivial.
    LoadOldPlan(HashSet<Ipv6Addr>),
    // Await any peers, as long as there are at least enough to make a new plan.
    CreateNewPlan(usize),
}

/// The implementation of the Rack Setup Service.
struct ServiceInner {
    log: Logger,
    dns_servers: OnceCell<DnsUpdater>,
}

impl ServiceInner {
    fn new(log: Logger) -> Self {
        ServiceInner { log, dns_servers: OnceCell::new() }
    }

    async fn initialize_datasets(
        &self,
        sled_address: SocketAddrV6,
        datasets: &Vec<DatasetEnsureBody>,
    ) -> Result<(), SetupServiceError> {
        let dur = std::time::Duration::from_secs(60);

        let client = reqwest::ClientBuilder::new()
            .connect_timeout(dur)
            .timeout(dur)
            .build()
            .map_err(SetupServiceError::HttpClient)?;
        let client = SledAgentClient::new_with_client(
            &format!("http://{}", sled_address),
            client,
            self.log.new(o!("SledAgentClient" => sled_address.to_string())),
        );

        info!(self.log, "sending dataset requests...");
        for dataset in datasets {
            let filesystem_put = || async {
                info!(self.log, "creating new filesystem: {:?}", dataset);
                client
                    .filesystem_put(&dataset.clone().into())
                    .await
                    .map_err(BackoffError::transient)?;
                Ok::<(), BackoffError<SledAgentError<SledAgentTypes::Error>>>(())
            };
            let log_failure = |error, _| {
                warn!(self.log, "failed to create filesystem"; "error" => ?error);
            };
            retry_notify(
                retry_policy_internal_service_aggressive(),
                filesystem_put,
                log_failure,
            )
            .await?;
        }

        let mut records: HashMap<_, Vec<_>> = HashMap::new();
        for dataset in datasets {
            records
                .entry(dataset.srv())
                .or_default()
                .push((dataset.aaaa(), dataset.address()));
        }
        let records_put = || async {
            self.dns_servers
                .get()
                .expect("DNS servers must be initialized first")
                .insert_dns_records(&records)
                .await
                .map_err(BackoffError::transient)?;
            Ok::<(), BackoffError<DnsError>>(())
        };
        let log_failure = |error, _| {
            warn!(self.log, "failed to set DNS records"; "error" => ?error);
        };
        retry_notify(
            retry_policy_internal_service_aggressive(),
            records_put,
            log_failure,
        )
        .await?;

        Ok(())
    }

    async fn initialize_services(
        &self,
        sled_address: SocketAddrV6,
        services: &Vec<ServiceZoneRequest>,
    ) -> Result<(), SetupServiceError> {
        let dur = std::time::Duration::from_secs(60);
        let client = reqwest::ClientBuilder::new()
            .connect_timeout(dur)
            .timeout(dur)
            .build()
            .map_err(SetupServiceError::HttpClient)?;
        let client = SledAgentClient::new_with_client(
            &format!("http://{}", sled_address),
            client,
            self.log.new(o!("SledAgentClient" => sled_address.to_string())),
        );

        info!(self.log, "sending service requests...");
        let services_put = || async {
            info!(self.log, "initializing sled services: {:?}", services);
            client
                .services_put(&SledAgentTypes::ServiceEnsureBody {
                    services: services
                        .iter()
                        .map(|s| s.clone().into())
                        .collect(),
                })
                .await
                .map_err(BackoffError::transient)?;
            Ok::<(), BackoffError<SledAgentError<SledAgentTypes::Error>>>(())
        };
        let log_failure = |error, _| {
            warn!(self.log, "failed to initialize services"; "error" => ?error);
        };
        retry_notify(
            retry_policy_internal_service_aggressive(),
            services_put,
            log_failure,
        )
        .await?;

        // Insert DNS records, if the DNS servers have been initialized
        if let Some(dns_servers) = self.dns_servers.get() {
            let mut records = HashMap::new();
            for zone in services {
                for service in &zone.services {
                    if let Some(addr) = zone.address(&service) {
                        records
                            .entry(zone.srv(&service))
                            .or_insert_with(Vec::new)
                            .push((zone.aaaa(), addr));
                    }
                }
            }
            let records_put = || async {
                dns_servers
                    .insert_dns_records(&records)
                    .await
                    .map_err(BackoffError::transient)?;
                Ok::<(), BackoffError<DnsError>>(())
            };
            let log_failure = |error, _| {
                warn!(self.log, "failed to set DNS records"; "error" => ?error);
            };
            retry_notify(
                retry_policy_internal_service_aggressive(),
                records_put,
                log_failure,
            )
            .await?;
        }

        Ok(())
    }

    // Waits for sufficient neighbors to exist so the initial set of requests
    // can be send out.
    async fn wait_for_peers(
        &self,
        expectation: PeerExpectation,
        our_bootstrap_address: Ipv6Addr,
    ) -> Result<Vec<Ipv6Addr>, DdmError> {
        let ddm_admin_client = DdmAdminClient::new(self.log.clone())?;
        let addrs = retry_notify(
            retry_policy_internal_service_aggressive(),
            || async {
                let peer_addrs =
                    ddm_admin_client.peer_addrs().await.map_err(|err| {
                        BackoffError::transient(format!(
                            "Failed getting peers from mg-ddm: {err}"
                        ))
                    })?;

                let all_addrs = peer_addrs
                    .chain(iter::once(our_bootstrap_address))
                    .collect::<HashSet<_>>();

                match expectation {
                    PeerExpectation::LoadOldPlan(ref expected) => {
                        if all_addrs.is_superset(expected) {
                            Ok(all_addrs.into_iter().collect())
                        } else {
                            Err(BackoffError::transient(
                                concat!(
                                    "Waiting for a LoadOldPlan set ",
                                    "of peers not found yet."
                                )
                                .to_string(),
                            ))
                        }
                    }
                    PeerExpectation::CreateNewPlan(wanted_peer_count) => {
                        if all_addrs.len() >= wanted_peer_count {
                            Ok(all_addrs.into_iter().collect())
                        } else {
                            Err(BackoffError::transient(format!(
                                "Waiting for {} peers (currently have {})",
                                wanted_peer_count,
                                all_addrs.len()
                            )))
                        }
                    }
                }
            },
            |message, duration| {
                info!(
                    self.log,
                    "{} (will retry after {:?})", message, duration
                );
            },
        )
        // `retry_policy_internal_service_aggressive()` retries indefinitely on transient errors
        // (the only kind we produce), allowing us to `.unwrap()` without
        // panicking
        .await
        .unwrap();

        Ok(addrs)
    }

    async fn handoff_to_nexus(
        &self,
        config: &Config,
        sled_plan: &SledPlan,
        service_plan: &ServicePlan,
    ) -> Result<(), SetupServiceError> {
        info!(self.log, "Handing off control to Nexus");

        let resolver = DnsResolver::new(&config.az_subnet())
            .expect("Failed to create DNS resolver");
        let ip = resolver
            .lookup_ip(SRV::Service(ServiceName::Nexus))
            .await
            .expect("Failed to lookup IP");
        let nexus_address = SocketAddr::new(ip, NEXUS_INTERNAL_PORT);

        info!(self.log, "Nexus address: {}", nexus_address.to_string());

        let nexus_client = NexusClient::new(
            &format!("http://{}", nexus_address),
            self.log.new(o!("component" => "NexusClient")),
        );

        // Ensure we can quickly look up "Sled Agent Address" -> "UUID of sled".
        //
        // We need the ID when passing info to Nexus.
        let mut id_map = HashMap::new();
        for (_, sled_request) in sled_plan.sleds.iter() {
            id_map
                .insert(get_sled_address(sled_request.subnet), sled_request.id);
        }

        // Convert all the information we have about services and datasets into
        // a format which can be processed by Nexus.
        let mut services: Vec<NexusTypes::ServicePutRequest> = vec![];
        let mut datasets: Vec<NexusTypes::DatasetCreateRequest> = vec![];
        let mut internal_services_ip_pool_ranges: Vec<NexusTypes::IpRange> =
            vec![];
        for (addr, service_request) in service_plan.services.iter() {
            let sled_id = *id_map
                .get(addr)
                .expect("Sled address in service plan, but not sled plan");

            for zone in &service_request.services {
                for svc in &zone.services {
                    let kind = match svc {
                        ServiceType::Nexus { external_ip, internal_ip: _ } => {
                            // NOTE: Eventually, this IP pool will be entirely
                            // user-supplied. For now, however, it's inferred
                            // based on the input IP addresses.
                            let range = match external_ip {
                                IpAddr::V4(addr) => NexusTypes::IpRange::V4(
                                    NexusTypes::Ipv4Range {
                                        first: *addr,
                                        last: *addr,
                                    },
                                ),
                                IpAddr::V6(addr) => NexusTypes::IpRange::V6(
                                    NexusTypes::Ipv6Range {
                                        first: *addr,
                                        last: *addr,
                                    },
                                ),
                            };
                            internal_services_ip_pool_ranges.push(range);

                            NexusTypes::ServiceKind::Nexus {
                                external_address: *external_ip,
                            }
                        }
                        ServiceType::InternalDns { .. } => {
                            NexusTypes::ServiceKind::InternalDNS
                        }
                        ServiceType::Oximeter => {
                            NexusTypes::ServiceKind::Oximeter
                        }
                        ServiceType::CruciblePantry => {
                            NexusTypes::ServiceKind::CruciblePantry
                        }
                        _ => {
                            return Err(SetupServiceError::BadConfig(format!(
                                "RSS should not request service of type: {}",
                                svc
                            )));
                        }
                    };

                    services.push(NexusTypes::ServicePutRequest {
                        service_id: zone.id,
                        sled_id,
                        // TODO: Should this be a vec, or a single value?
                        address: zone.addresses[0],
                        kind,
                    })
                }
            }

            for dataset in service_request.datasets.iter() {
                datasets.push(NexusTypes::DatasetCreateRequest {
                    zpool_id: dataset.zpool_id,
                    dataset_id: dataset.id,
                    request: NexusTypes::DatasetPutRequest {
                        address: dataset.address.to_string(),
                        kind: dataset.dataset_kind.clone().into(),
                    },
                })
            }
        }

        let request = NexusTypes::RackInitializationRequest {
            services,
            datasets,
            // TODO(https://github.com/oxidecomputer/omicron/issues/1530): Plumb
            // these pools through RSS's API.
            //
            // Currently, we're passing the addresses to accomodate Nexus
            // services, but the operator may want to supply additional
            // addresses.
            internal_services_ip_pool_ranges,
            // TODO(https://github.com/oxidecomputer/omicron/issues/1959): Plumb
            // these paths through RSS's API.
            //
            // These certificates CAN be updated through Nexus' HTTP API, but
            // should be bootstrapped during the rack setup process to avoid
            // the need for unencrypted communication.
            certs: vec![],
        };

        let notify_nexus = || async {
            nexus_client
                .rack_initialization_complete(&sled_plan.rack_id, &request)
                .await
                .map_err(BackoffError::transient)
        };
        let log_failure = |err, _| {
            info!(self.log, "Failed to handoff to nexus: {err}");
        };

        retry_notify(
            retry_policy_internal_service_aggressive(),
            notify_nexus,
            log_failure,
        )
        .await?;

        info!(self.log, "Handoff to Nexus is complete");
        Ok(())
    }

    // In lieu of having an operator send requests to all sleds via an
    // initialization service, the sled-agent configuration may allow for the
    // automated injection of setup requests from a sled.
    //
    // This method has a few distinct phases, identified by files in durable
    // storage:
    //
    // 1. SLED ALLOCATION PLAN CREATION. When the RSS starts up for the first
    //    time, it creates an allocation plan to provision subnets to an initial
    //    set of sleds.
    //
    // 2. SLED ALLOCATION PLAN EXECUTION. The RSS then carries out this plan, making
    //    requests to the sleds enumerated within the "allocation plan".
    //
    // 3. SERVICE ALLOCATION PLAN CREATION. Now that Sled Agents are executing
    //    on their respsective subnets, they can be queried to create an
    //    allocation plan for services.
    //
    // 4. SERVICE ALLOCATION PLAN EXECUTION. RSS requests that the services
    //    outlined in the aforementioned step are created.
    //
    // 5. MARKING SETUP COMPLETE. Once the RSS has successfully initialized the
    //    rack, a marker file is created at "rss_completed_marker_path()". This
    //    indicates that the plan executed successfully, and no work remains.
    async fn inject_rack_setup_requests(
        &self,
        config: &Config,
        local_bootstrap_agent: BootstrapAgentHandle,
        member_device_id_certs: &[Ed25519Certificate],
    ) -> Result<(), SetupServiceError> {
        info!(self.log, "Injecting RSS configuration: {:#?}", config);

        // Check if a previous RSS plan has completed successfully.
        //
        // If it has, the system should be up-and-running.
        let rss_completed_marker_path = rss_completed_marker_path();
        if rss_completed_marker_path.exists() {
            // TODO(https://github.com/oxidecomputer/omicron/issues/724): If the
            // running configuration doesn't match Config, we could try to
            // update things.
            info!(
                self.log,
                "RSS configuration looks like it has already been applied",
            );

            let sled_plan = SledPlan::load(&self.log)
                .await?
                .expect("Sled plan should exist if completed marker exists");
            if &sled_plan.config != config {
                return Err(SetupServiceError::BadConfig(
                    "Configuration changed".to_string(),
                ));
            }
            let service_plan = ServicePlan::load(&self.log)
                .await?
                .expect("Service plan should exist if completed marker exists");
            self.handoff_to_nexus(&config, &sled_plan, &service_plan).await?;
            return Ok(());
        } else {
            info!(self.log, "RSS configuration has not been fully applied yet",);
        }

        // Wait for either:
        // - All the peers to re-load an old plan (if one exists)
        // - Enough peers to create a new plan (if one does not exist)
        let maybe_sled_plan = SledPlan::load(&self.log).await?;
        let expectation = if let Some(plan) = &maybe_sled_plan {
            PeerExpectation::LoadOldPlan(
                plan.sleds.keys().map(|a| *a.ip()).collect(),
            )
        } else {
            PeerExpectation::CreateNewPlan(MINIMUM_SLED_COUNT)
        };
        let addrs = self
            .wait_for_peers(expectation, local_bootstrap_agent.our_address())
            .await?;
        info!(self.log, "Enough peers exist to enact RSS plan");

        // If we created a plan, reuse it. Otherwise, create a new plan.
        //
        // NOTE: This is a "point-of-no-return" -- before sending any requests
        // to neighboring sleds, the plan must be recorded to durable storage.
        // This way, if the RSS power-cycles, it can idempotently provide the
        // same subnets to the same sleds.
        let plan = if let Some(plan) = maybe_sled_plan {
            info!(self.log, "Re-using existing allocation plan");
            plan
        } else {
            info!(self.log, "Creating new allocation plan");
            SledPlan::create(&self.log, config, addrs).await?
        };
        let config = &plan.config;

        // Generate our rack secret, unless we're in the single-sled case.
        let mut maybe_rack_secret_shares = generate_rack_secret(
            config.rack_secret_threshold,
            member_device_id_certs,
            &self.log,
        )?;

        // Confirm that the returned iterator (if we got one) is the length we
        // expect.
        if let Some(rack_secret_shares) = maybe_rack_secret_shares.as_ref() {
            // TODO-cleanup Asserting here seems fine as long as
            // `member_device_id_certs` is hard-coded from a config file, but
            // once we start collecting them over the management network we
            // should probably attach them at the type level to the bootstrap
            // addrs, which would remove the need for this assertion.
            assert_eq!(
                rack_secret_shares.len(),
                plan.sleds.len(),
                concat!(
                    "Number of trust quorum members does not match ",
                    "number of sleds in the plan"
                )
            );
        }

        // Forward the sled initialization requests to our sled-agent.
        local_bootstrap_agent
            .initialize_sleds(
                plan.sleds
                    .iter()
                    .map(move |(bootstrap_addr, initialization_request)| {
                        (
                            *bootstrap_addr,
                            initialization_request.clone(),
                            maybe_rack_secret_shares
                                .as_mut()
                                .map(|shares| shares.next().unwrap()),
                        )
                    })
                    .collect(),
            )
            .await
            .map_err(SetupServiceError::SledInitialization)?;

        // Now that sled agents have been initialized, we can create
        // a service allocation plan.
        let sled_addresses: Vec<_> = plan
            .sleds
            .values()
            .map(|initialization_request| {
                get_sled_address(initialization_request.subnet)
            })
            .collect();
        let service_plan =
            if let Some(plan) = ServicePlan::load(&self.log).await? {
                plan
            } else {
                ServicePlan::create(&self.log, &config, &sled_addresses).await?
            };

        // Set up internal DNS services.
        futures::future::join_all(service_plan.services.iter().map(
            |(sled_address, services_request)| async move {
                let dns_services: Vec<_> = services_request
                    .services
                    .iter()
                    .filter_map(|svc| {
                        if matches!(svc.zone_type, ZoneType::InternalDNS) {
                            Some(svc.clone())
                        } else {
                            None
                        }
                    })
                    .collect();
                if !dns_services.is_empty() {
                    self.initialize_services(*sled_address, &dns_services)
                        .await?;
                }
                Ok(())
            },
        ))
        .await
        .into_iter()
        .collect::<Result<_, SetupServiceError>>()?;

        let dns_servers = DnsUpdater::new(
            &config.az_subnet(),
            self.log.new(o!("client" => "DNS")),
        );
        self.dns_servers
            .set(dns_servers)
            .map_err(|_| ())
            .expect("DNS servers should only be set once");

        // Issue the dataset initialization requests to all sleds.
        futures::future::join_all(service_plan.services.iter().map(
            |(sled_address, services_request)| async move {
                self.initialize_datasets(
                    *sled_address,
                    &services_request.datasets,
                )
                .await?;
                Ok(())
            },
        ))
        .await
        .into_iter()
        .collect::<Result<_, SetupServiceError>>()?;

        info!(self.log, "Finished setting up agents and datasets");

        // Issue service initialization requests.
        //
        // NOTE: This must happen *after* the dataset initialization,
        // to ensure that CockroachDB has been initialized before Nexus
        // starts.
        //
        // If Nexus was more resilient to concurrent initialization
        // of CRDB, this requirement could be relaxed.
        futures::future::join_all(service_plan.services.iter().map(
            |(sled_address, services_request)| async move {
                // With the current implementation of "initialize_services",
                // we must provide the set of *all* services that should be
                // executing on a sled.
                //
                // This means re-requesting the DNS service, even if it is
                // already running - this is fine, however, as the receiving
                // sled agent doesn't modify the already-running service.
                self.initialize_services(
                    *sled_address,
                    &services_request.services,
                )
                .await?;
                Ok(())
            },
        ))
        .await
        .into_iter()
        .collect::<Result<Vec<()>, SetupServiceError>>()?;

        info!(self.log, "Finished setting up services");

        // Finally, mark that we've completed executing the plans.
        tokio::fs::File::create(&rss_completed_marker_path).await.map_err(
            |err| SetupServiceError::Io {
                message: format!("creating {rss_completed_marker_path:?}"),
                err,
            },
        )?;

        // At this point, even if we reboot, we must not try to manage sleds,
        // services, or DNS records.
        self.handoff_to_nexus(&config, &plan, &service_plan).await?;

        // TODO Questions to consider:
        // - What if a sled comes online *right after* this setup? How does
        // it get a /64?

        Ok(())
    }
}
