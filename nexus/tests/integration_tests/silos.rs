// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::integration_tests::saml::SAML_IDP_DESCRIPTOR;
use nexus_test_utils::http_testing::{AuthnMode, NexusRequest, RequestBuilder};
use nexus_test_utils::resource_helpers::{
    create_local_user, create_organization, create_silo, grant_iam,
    object_create, objects_list_page_authz,
};
use nexus_test_utils_macros::nexus_test;
use omicron_common::api::external::ObjectIdentity;
use omicron_common::api::external::{
    IdentityMetadataCreateParams, LookupType, Name,
};
use omicron_nexus::authn::silos::{AuthenticatedSubject, IdentityProviderType};
use omicron_nexus::authn::{USER_TEST_PRIVILEGED, USER_TEST_UNPRIVILEGED};
use omicron_nexus::authz::{self, SiloRole};
use omicron_nexus::context::OpContext;
use omicron_nexus::db;
use omicron_nexus::db::fixed_data::silo::{DEFAULT_SILO, SILO_ID};
use omicron_nexus::db::identity::Asset;
use omicron_nexus::db::lookup::LookupPath;
use omicron_nexus::external_api::views::{
    self, IdentityProvider, Organization, SamlIdentityProvider, Silo,
};
use omicron_nexus::external_api::{params, shared};

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write;
use std::str::FromStr;

use base64::Engine;
use http::method::Method;
use http::StatusCode;
use httptest::{matchers::*, responders::*, Expectation, Server};
use uuid::Uuid;

type ControlPlaneTestContext =
    nexus_test_utils::ControlPlaneTestContext<omicron_nexus::Server>;

#[nexus_test]
async fn test_silos(cptestctx: &ControlPlaneTestContext) {
    let client = &cptestctx.external_client;
    let nexus = &cptestctx.server.apictx().nexus;

    // Create two silos: one discoverable, one not
    create_silo(
        &client,
        "discoverable",
        true,
        shared::SiloIdentityMode::LocalOnly,
    )
    .await;
    create_silo(&client, "hidden", false, shared::SiloIdentityMode::LocalOnly)
        .await;

    // Verify GET /system/silos/{silo} works for both discoverable and not
    let discoverable_url = "/system/silos/discoverable";
    let hidden_url = "/system/silos/hidden";

    let silo: Silo = NexusRequest::object_get(&client, &discoverable_url)
        .authn_as(AuthnMode::PrivilegedUser)
        .execute()
        .await
        .expect("failed to make request")
        .parsed_body()
        .unwrap();
    assert_eq!(silo.identity.name, "discoverable");

    let silo: Silo = NexusRequest::object_get(&client, &hidden_url)
        .authn_as(AuthnMode::PrivilegedUser)
        .execute()
        .await
        .expect("failed to make request")
        .parsed_body()
        .unwrap();
    assert_eq!(silo.identity.name, "hidden");

    // Verify 404 if silo doesn't exist
    NexusRequest::expect_failure(
        &client,
        StatusCode::NOT_FOUND,
        Method::GET,
        &"/system/silos/testpost",
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .expect("failed to make request");

    // Verify GET /system/silos only returns discoverable silos
    let silos =
        objects_list_page_authz::<Silo>(client, "/system/silos").await.items;
    assert_eq!(silos.len(), 1);
    assert_eq!(silos[0].identity.name, "discoverable");

    // Create a new user in the discoverable silo
    let new_silo_user_id = create_local_user(
        client,
        &silos[0],
        &"some-silo-user".parse().unwrap(),
        params::UserPassword::InvalidPassword,
    )
    .await
    .id;

    // Grant the user "admin" privileges on that Silo.
    grant_iam(
        client,
        "/system/silos/discoverable",
        SiloRole::Admin,
        new_silo_user_id,
        AuthnMode::PrivilegedUser,
    )
    .await;

    // TODO-coverage  TODO-security Add test for Silo-local session
    // when we can use users in another Silo.

    let authn_opctx = nexus.opctx_external_authn();

    // Create organization with built-in user auth
    // Note: this currently goes to the built-in silo!
    let org_name: Name = "someorg".parse().unwrap();
    let new_org_in_default_silo =
        create_organization(&client, org_name.as_str()).await;

    // Create an Organization of the same name in a different Silo to verify
    // that's possible.
    let new_org_in_our_silo = NexusRequest::objects_post(
        client,
        "/organizations",
        &params::OrganizationCreate {
            identity: IdentityMetadataCreateParams {
                name: org_name.clone(),
                description: String::new(),
            },
        },
    )
    .authn_as(AuthnMode::SiloUser(new_silo_user_id))
    .execute()
    .await
    .expect("failed to create same-named Organization in a different Silo")
    .parsed_body::<views::Organization>()
    .expect("failed to parse new Organization");
    assert_eq!(
        new_org_in_default_silo.identity.name,
        new_org_in_our_silo.identity.name
    );
    assert_ne!(
        new_org_in_default_silo.identity.id,
        new_org_in_our_silo.identity.id
    );
    // Delete it so that we can delete the Silo later.
    NexusRequest::object_delete(
        client,
        &format!("/organizations/{}", org_name),
    )
    .authn_as(AuthnMode::SiloUser(new_silo_user_id))
    .execute()
    .await
    .expect("failed to delete test Organization");

    // Verify GET /organizations works with built-in user auth
    let organizations =
        objects_list_page_authz::<Organization>(client, "/organizations")
            .await
            .items;
    assert_eq!(organizations.len(), 1);
    assert_eq!(organizations[0].identity.name, "someorg");

    // TODO: uncomment when silo users can have role assignments
    /*
    // Verify GET /organizations doesn't list anything if authing under
    // different silo.
    let organizations =
        objects_list_page_authz_with_session::<Organization>(
            client, "/organizations", &session,
        )
        .await
        .items;
    assert_eq!(organizations.len(), 0);
    */

    // Verify DELETE doesn't work if organizations exist
    // TODO: put someorg in discoverable silo, not built-in
    NexusRequest::expect_failure(
        &client,
        StatusCode::BAD_REQUEST,
        Method::DELETE,
        &"/system/silos/default-silo",
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .expect("failed to make request");

    // Delete organization
    NexusRequest::object_delete(&client, &"/organizations/someorg")
        .authn_as(AuthnMode::PrivilegedUser)
        .execute()
        .await
        .expect("failed to make request");

    // Verify silo DELETE works
    NexusRequest::object_delete(&client, &"/system/silos/discoverable")
        .authn_as(AuthnMode::PrivilegedUser)
        .execute()
        .await
        .expect("failed to make request");

    // Verify silo user was also deleted
    LookupPath::new(&authn_opctx, nexus.datastore())
        .silo_user_id(new_silo_user_id)
        .fetch()
        .await
        .expect_err("unexpected success");
}

// Test that admin group is created if admin_group_name is applied.
#[nexus_test]
async fn test_silo_admin_group(cptestctx: &ControlPlaneTestContext) {
    let client = &cptestctx.external_client;
    let nexus = &cptestctx.server.apictx().nexus;

    let silo: Silo = object_create(
        client,
        "/system/silos",
        &params::SiloCreate {
            identity: IdentityMetadataCreateParams {
                name: "silo-name".parse().unwrap(),
                description: "a silo".to_string(),
            },
            discoverable: false,
            identity_mode: shared::SiloIdentityMode::SamlJit,
            admin_group_name: Some("administrator".into()),
        },
    )
    .await;

    let authn_opctx = nexus.opctx_external_authn();

    let (authz_silo, db_silo) =
        LookupPath::new(&authn_opctx, &nexus.datastore())
            .silo_name(&silo.identity.name.into())
            .fetch()
            .await
            .unwrap();

    assert!(nexus
        .datastore()
        .silo_group_optional_lookup(
            &authn_opctx,
            &authz_silo,
            "administrator".into(),
        )
        .await
        .unwrap()
        .is_some());

    // Test that a user is granted privileges from their group membership
    let admin_group_user = nexus
        .silo_user_from_authenticated_subject(
            &authn_opctx,
            &authz_silo,
            &db_silo,
            &AuthenticatedSubject {
                external_id: "adminuser@company.com".into(),
                groups: vec!["administrator".into()],
            },
        )
        .await
        .unwrap()
        .unwrap();

    let group_memberships = nexus
        .datastore()
        .silo_group_membership_for_user(
            &authn_opctx,
            &authz_silo,
            admin_group_user.id(),
        )
        .await
        .unwrap();

    assert_eq!(group_memberships.len(), 1);

    // Create an organization
    let _org = NexusRequest::objects_post(
        client,
        "/organizations",
        &params::OrganizationCreate {
            identity: IdentityMetadataCreateParams {
                name: "myorg".parse().unwrap(),
                description: "some org".into(),
            },
        },
    )
    .authn_as(AuthnMode::SiloUser(admin_group_user.id()))
    .execute()
    .await
    .expect("failed to create Organization")
    .parsed_body::<views::Organization>()
    .expect("failed to parse as Organization");
}

// Test listing providers
#[nexus_test]
async fn test_listing_identity_providers(cptestctx: &ControlPlaneTestContext) {
    let client = &cptestctx.external_client;
    create_silo(&client, "test-silo", true, shared::SiloIdentityMode::SamlJit)
        .await;

    // List providers - should be none
    let providers = objects_list_page_authz::<IdentityProvider>(
        client,
        "/system/silos/test-silo/identity-providers",
    )
    .await
    .items;

    assert_eq!(providers.len(), 0);

    // Add some providers
    let saml_idp_descriptor = SAML_IDP_DESCRIPTOR;

    let server = Server::run();
    server.expect(
        Expectation::matching(request::method_path("GET", "/descriptor"))
            .times(1..)
            .respond_with(status_code(200).body(saml_idp_descriptor)),
    );

    let silo_saml_idp_1: SamlIdentityProvider = object_create(
        client,
        &"/system/silos/test-silo/identity-providers/saml",
        &params::SamlIdentityProviderCreate {
            identity: IdentityMetadataCreateParams {
                name: "some-totally-real-saml-provider"
                    .to_string()
                    .parse()
                    .unwrap(),
                description: "a demo provider".to_string(),
            },

            idp_metadata_source: params::IdpMetadataSource::Url {
                url: server.url("/descriptor").to_string(),
            },

            idp_entity_id: "entity_id".to_string(),
            sp_client_id: "client_id".to_string(),
            acs_url: "http://acs".to_string(),
            slo_url: "http://slo".to_string(),
            technical_contact_email: "technical@fake".to_string(),

            signing_keypair: None,

            group_attribute_name: None,
        },
    )
    .await;

    let silo_saml_idp_2: SamlIdentityProvider = object_create(
        client,
        &"/system/silos/test-silo/identity-providers/saml",
        &params::SamlIdentityProviderCreate {
            identity: IdentityMetadataCreateParams {
                name: "another-totally-real-saml-provider"
                    .to_string()
                    .parse()
                    .unwrap(),
                description: "a demo provider".to_string(),
            },

            idp_metadata_source: params::IdpMetadataSource::Url {
                url: server.url("/descriptor").to_string(),
            },

            idp_entity_id: "entity_id".to_string(),
            sp_client_id: "client_id".to_string(),
            acs_url: "http://acs".to_string(),
            slo_url: "http://slo".to_string(),
            technical_contact_email: "technical@fake".to_string(),

            signing_keypair: None,

            group_attribute_name: None,
        },
    )
    .await;

    // List providers again - expect 2
    let providers = objects_list_page_authz::<IdentityProvider>(
        client,
        "/system/silos/test-silo/identity-providers",
    )
    .await
    .items;

    assert_eq!(providers.len(), 2);

    let provider_name_set =
        providers.into_iter().map(|x| x.identity.name).collect::<HashSet<_>>();
    assert!(provider_name_set.contains(&silo_saml_idp_1.identity.name));
    assert!(provider_name_set.contains(&silo_saml_idp_2.identity.name));
}

// Test that deleting the silo deletes the idp
#[nexus_test]
async fn test_deleting_a_silo_deletes_the_idp(
    cptestctx: &ControlPlaneTestContext,
) {
    let client = &cptestctx.external_client;

    const SILO_NAME: &str = "test-silo";
    create_silo(&client, SILO_NAME, true, shared::SiloIdentityMode::SamlJit)
        .await;

    let saml_idp_descriptor = SAML_IDP_DESCRIPTOR;

    let server = Server::run();
    server.expect(
        Expectation::matching(request::method_path("GET", "/descriptor"))
            .respond_with(status_code(200).body(saml_idp_descriptor)),
    );

    let silo_saml_idp: SamlIdentityProvider = object_create(
        client,
        &format!("/system/silos/{}/identity-providers/saml", SILO_NAME),
        &params::SamlIdentityProviderCreate {
            identity: IdentityMetadataCreateParams {
                name: "some-totally-real-saml-provider"
                    .to_string()
                    .parse()
                    .unwrap(),
                description: "a demo provider".to_string(),
            },

            idp_metadata_source: params::IdpMetadataSource::Url {
                url: server.url("/descriptor").to_string(),
            },

            idp_entity_id: "entity_id".to_string(),
            sp_client_id: "client_id".to_string(),
            acs_url: "http://acs".to_string(),
            slo_url: "http://slo".to_string(),
            technical_contact_email: "technical@fake".to_string(),

            signing_keypair: None,

            group_attribute_name: None,
        },
    )
    .await;

    // Delete the silo
    NexusRequest::object_delete(
        &client,
        &format!("/system/silos/{}", SILO_NAME),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .expect("failed to make request");

    // Expect that the silo is gone
    let nexus = &cptestctx.server.apictx().nexus;

    let response = IdentityProviderType::lookup(
        &nexus.datastore(),
        &nexus.opctx_external_authn(),
        &omicron_common::api::external::Name::try_from(SILO_NAME.to_string())
            .unwrap()
            .into(),
        &omicron_common::api::external::Name::try_from(
            "some-totally-real-saml-provider".to_string(),
        )
        .unwrap()
        .into(),
    )
    .await;

    assert!(response.is_err());
    match response.err().unwrap() {
        omicron_common::api::external::Error::ObjectNotFound {
            type_name,
            lookup_type: _,
        } => {
            assert_eq!(
                type_name,
                omicron_common::api::external::ResourceType::Silo
            );
        }

        _ => {
            assert!(false);
        }
    }

    // No SSO redirect expected
    NexusRequest::new(
        RequestBuilder::new(
            client,
            Method::GET,
            &format!(
                "/login/{}/saml/{}",
                SILO_NAME, silo_saml_idp.identity.name
            ),
        )
        .expect_status(Some(StatusCode::NOT_FOUND)),
    )
    .execute()
    .await
    .expect("expected success");
}

// Create a Silo with a SAML IdP document string
#[nexus_test]
async fn test_saml_idp_metadata_data_valid(
    cptestctx: &ControlPlaneTestContext,
) {
    let client = &cptestctx.external_client;

    create_silo(&client, "blahblah", true, shared::SiloIdentityMode::SamlJit)
        .await;

    let silo_saml_idp: SamlIdentityProvider = object_create(
        client,
        "/system/silos/blahblah/identity-providers/saml",
        &params::SamlIdentityProviderCreate {
            identity: IdentityMetadataCreateParams {
                name: "some-totally-real-saml-provider"
                    .to_string()
                    .parse()
                    .unwrap(),
                description: "a demo provider".to_string(),
            },

            idp_metadata_source: params::IdpMetadataSource::Base64EncodedXml {
                data: base64::engine::general_purpose::STANDARD
                    .encode(SAML_IDP_DESCRIPTOR),
            },

            idp_entity_id: "entity_id".to_string(),
            sp_client_id: "client_id".to_string(),
            acs_url: "http://acs".to_string(),
            slo_url: "http://slo".to_string(),
            technical_contact_email: "technical@fake".to_string(),

            signing_keypair: None,

            group_attribute_name: None,
        },
    )
    .await;

    // Expect the SSO redirect when trying to log in
    let result = NexusRequest::new(
        RequestBuilder::new(
            client,
            Method::GET,
            &format!("/login/blahblah/saml/{}", silo_saml_idp.identity.name),
        )
        .expect_status(Some(StatusCode::FOUND)),
    )
    .execute()
    .await
    .expect("expected success");

    assert!(result.headers["Location"]
        .to_str()
        .unwrap()
        .to_string()
        .starts_with(
            "https://idp.example.org/SAML2/SSO/Redirect?SAMLRequest=",
        ));
}

// Fail to create a Silo with a SAML IdP document string that isn't valid
#[nexus_test]
async fn test_saml_idp_metadata_data_truncated(
    cptestctx: &ControlPlaneTestContext,
) {
    let client = &cptestctx.external_client;

    create_silo(&client, "blahblah", true, shared::SiloIdentityMode::SamlJit)
        .await;

    NexusRequest::new(
        RequestBuilder::new(
            client,
            Method::POST,
            "/system/silos/blahblah/identity-providers/saml",
        )
        .body(Some(&params::SamlIdentityProviderCreate {
            identity: IdentityMetadataCreateParams {
                name: "some-totally-real-saml-provider"
                    .to_string()
                    .parse()
                    .unwrap(),
                description: "a demo provider".to_string(),
            },

            idp_metadata_source: params::IdpMetadataSource::Base64EncodedXml {
                data: base64::engine::general_purpose::STANDARD.encode({
                    let mut saml_idp_descriptor =
                        SAML_IDP_DESCRIPTOR.to_string();
                    saml_idp_descriptor.truncate(100);
                    saml_idp_descriptor
                }),
            },

            idp_entity_id: "entity_id".to_string(),
            sp_client_id: "client_id".to_string(),
            acs_url: "http://acs".to_string(),
            slo_url: "http://slo".to_string(),
            technical_contact_email: "technical@fake".to_string(),

            signing_keypair: None,

            group_attribute_name: None,
        }))
        .expect_status(Some(StatusCode::BAD_REQUEST)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .expect("unexpected success");
}

// Can't create a SAML IdP from bad base64 data
#[nexus_test]
async fn test_saml_idp_metadata_data_invalid(
    cptestctx: &ControlPlaneTestContext,
) {
    let client = &cptestctx.external_client;

    const SILO_NAME: &str = "saml-silo";
    create_silo(&client, SILO_NAME, true, shared::SiloIdentityMode::SamlJit)
        .await;

    NexusRequest::new(
        RequestBuilder::new(
            client,
            Method::POST,
            &format!("/system/silos/{}/identity-providers/saml", SILO_NAME),
        )
        .body(Some(&params::SamlIdentityProviderCreate {
            identity: IdentityMetadataCreateParams {
                name: "some-totally-real-saml-provider"
                    .to_string()
                    .parse()
                    .unwrap(),
                description: "a demo provider".to_string(),
            },

            idp_metadata_source: params::IdpMetadataSource::Base64EncodedXml {
                data: "bad data".to_string(),
            },

            idp_entity_id: "entity_id".to_string(),
            sp_client_id: "client_id".to_string(),
            acs_url: "http://acs".to_string(),
            slo_url: "http://slo".to_string(),
            technical_contact_email: "technical@fake".to_string(),

            signing_keypair: None,

            group_attribute_name: None,
        }))
        .expect_status(Some(StatusCode::BAD_REQUEST)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .expect("unexpected success");
}

struct TestSiloUserProvisionTypes {
    identity_mode: shared::SiloIdentityMode,
    existing_silo_user: bool,
    expect_user: bool,
}

#[nexus_test]
async fn test_silo_user_provision_types(cptestctx: &ControlPlaneTestContext) {
    let client = &cptestctx.external_client;
    let nexus = &cptestctx.server.apictx().nexus;
    let datastore = nexus.datastore();

    let test_cases: Vec<TestSiloUserProvisionTypes> = vec![
        // A silo configured with a "ApiOnly" user provision type should fetch a
        // user if it exists already.
        TestSiloUserProvisionTypes {
            identity_mode: shared::SiloIdentityMode::LocalOnly,
            existing_silo_user: true,
            expect_user: true,
        },
        // A silo configured with a "ApiOnly" user provision type should not
        // create a user if one does not exist already.
        TestSiloUserProvisionTypes {
            identity_mode: shared::SiloIdentityMode::LocalOnly,
            existing_silo_user: false,
            expect_user: false,
        },
        // A silo configured with a "JIT" user provision type should fetch a
        // user if it exists already.
        TestSiloUserProvisionTypes {
            identity_mode: shared::SiloIdentityMode::SamlJit,
            existing_silo_user: true,
            expect_user: true,
        },
        // A silo configured with a "JIT" user provision type should create a
        // user if one does not exist already.
        TestSiloUserProvisionTypes {
            identity_mode: shared::SiloIdentityMode::SamlJit,
            existing_silo_user: false,
            expect_user: true,
        },
    ];

    for test_case in test_cases {
        let silo =
            create_silo(&client, "test-silo", true, test_case.identity_mode)
                .await;

        if test_case.existing_silo_user {
            match test_case.identity_mode {
                shared::SiloIdentityMode::SamlJit => {
                    create_jit_user(datastore, &silo, "external-id-com").await;
                }
                shared::SiloIdentityMode::LocalOnly => {
                    create_local_user(
                        client,
                        &silo,
                        &"external-id-com".parse().unwrap(),
                        params::UserPassword::InvalidPassword,
                    )
                    .await;
                }
            };
        }

        let authn_opctx = nexus.opctx_external_authn();

        let (authz_silo, db_silo) =
            LookupPath::new(&authn_opctx, &nexus.datastore())
                .silo_name(&silo.identity.name.into())
                .fetch()
                .await
                .unwrap();

        let existing_silo_user = nexus
            .silo_user_from_authenticated_subject(
                &authn_opctx,
                &authz_silo,
                &db_silo,
                &AuthenticatedSubject {
                    external_id: "external-id-com".into(),
                    groups: vec![],
                },
            )
            .await
            .unwrap();

        if test_case.expect_user {
            assert!(existing_silo_user.is_some());
        } else {
            assert!(existing_silo_user.is_none());
        }

        NexusRequest::object_delete(&client, &"/system/silos/test-silo")
            .authn_as(AuthnMode::PrivilegedUser)
            .execute()
            .await
            .expect("failed to make request");
    }
}

#[nexus_test]
async fn test_silo_user_fetch_by_external_id(
    cptestctx: &ControlPlaneTestContext,
) {
    let client = &cptestctx.external_client;
    let nexus = &cptestctx.server.apictx().nexus;

    let silo = create_silo(
        &client,
        "test-silo",
        true,
        shared::SiloIdentityMode::LocalOnly,
    )
    .await;

    let opctx_external_authn = nexus.opctx_external_authn();
    let opctx = OpContext::for_tests(
        cptestctx.logctx.log.new(o!()),
        nexus.datastore().clone(),
    );

    let (authz_silo, _) = LookupPath::new(&opctx, &nexus.datastore())
        .silo_name(&Name::try_from("test-silo".to_string()).unwrap().into())
        .fetch_for(authz::Action::Read)
        .await
        .unwrap();

    // Create a user
    create_local_user(
        client,
        &silo,
        &"f5513e049dac9468de5bdff36ab17d04f".parse().unwrap(),
        params::UserPassword::InvalidPassword,
    )
    .await;

    // Fetching by external id that's not in the db should be Ok(None)
    let result = nexus
        .datastore()
        .silo_user_fetch_by_external_id(
            &opctx_external_authn,
            &authz_silo,
            "123",
        )
        .await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_none());

    // Fetching by external id that is should be Ok(Some)
    let result = nexus
        .datastore()
        .silo_user_fetch_by_external_id(
            &opctx_external_authn,
            &authz_silo,
            "f5513e049dac9468de5bdff36ab17d04f",
        )
        .await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_some());
}

#[nexus_test]
async fn test_silo_users_list(cptestctx: &ControlPlaneTestContext) {
    let client = &cptestctx.external_client;

    let initial_silo_users: Vec<views::User> =
        NexusRequest::iter_collection_authn(client, "/users", "", None)
            .await
            .expect("failed to list silo users (1)")
            .all_items;

    // In the built-in Silo, we expect the test-privileged and test-unprivileged
    // users.
    assert_eq!(
        initial_silo_users,
        vec![
            views::User {
                id: USER_TEST_PRIVILEGED.id(),
                display_name: USER_TEST_PRIVILEGED.external_id.clone(),
                silo_id: *SILO_ID,
            },
            views::User {
                id: USER_TEST_UNPRIVILEGED.id(),
                display_name: USER_TEST_UNPRIVILEGED.external_id.clone(),
                silo_id: *SILO_ID,
            },
        ]
    );

    // Now create another user and make sure we can see them.  While we're at
    // it, use a small limit to check that pagination is really working.
    let new_silo_user_external_id = "can-we-see-them";
    let new_silo_user_id = create_local_user(
        client,
        &views::Silo::try_from(DEFAULT_SILO.clone()).unwrap(),
        &new_silo_user_external_id.parse().unwrap(),
        params::UserPassword::InvalidPassword,
    )
    .await
    .id;

    let mut silo_users: Vec<views::User> =
        NexusRequest::iter_collection_authn(client, "/users", "", Some(1))
            .await
            .expect("failed to list silo users (2)")
            .all_items;
    silo_users.sort_by(|u1, u2| u1.display_name.cmp(&u2.display_name));
    assert_eq!(
        silo_users,
        vec![
            views::User {
                id: new_silo_user_id,
                display_name: new_silo_user_external_id.into(),
                silo_id: *SILO_ID,
            },
            views::User {
                id: USER_TEST_PRIVILEGED.id(),
                display_name: USER_TEST_PRIVILEGED.external_id.clone(),
                silo_id: *SILO_ID,
            },
            views::User {
                id: USER_TEST_UNPRIVILEGED.id(),
                display_name: USER_TEST_UNPRIVILEGED.external_id.clone(),
                silo_id: *SILO_ID,
            },
        ]
    );

    // Create another Silo with a Silo administrator.  That user should not be
    // able to see the users in the first Silo.

    let silo =
        create_silo(client, "silo2", true, shared::SiloIdentityMode::LocalOnly)
            .await;

    let new_silo_user_name = String::from("some-silo-user");
    let new_silo_user_id = create_local_user(
        client,
        &silo,
        &new_silo_user_name.parse().unwrap(),
        params::UserPassword::InvalidPassword,
    )
    .await
    .id;
    grant_iam(
        client,
        "/system/silos/silo2",
        SiloRole::Admin,
        new_silo_user_id,
        AuthnMode::PrivilegedUser,
    )
    .await;

    let silo2_users: dropshot::ResultsPage<views::User> =
        NexusRequest::object_get(client, "/users")
            .authn_as(AuthnMode::SiloUser(new_silo_user_id))
            .execute()
            .await
            .unwrap()
            .parsed_body()
            .unwrap();
    assert_eq!(
        silo2_users.items,
        vec![views::User {
            id: new_silo_user_id,
            display_name: new_silo_user_name,
            silo_id: silo.identity.id,
        }]
    );

    // The "test-privileged" user also shouldn't see the user in this other
    // Silo.
    let mut new_silo_users: Vec<views::User> =
        NexusRequest::iter_collection_authn(client, "/users", "", Some(1))
            .await
            .expect("failed to list silo users (2)")
            .all_items;
    new_silo_users.sort_by(|u1, u2| u1.display_name.cmp(&u2.display_name));
    assert_eq!(silo_users, new_silo_users,);

    // TODO-coverage When we have a way to remove or invalidate Silo Users, we
    // should test that doing so causes them to stop appearing in the list.
}

#[nexus_test]
async fn test_silo_groups_jit(cptestctx: &ControlPlaneTestContext) {
    let client = &cptestctx.external_client;
    let nexus = &cptestctx.server.apictx().nexus;
    let datastore = nexus.datastore();

    let silo = create_silo(
        &client,
        "test-silo",
        true,
        shared::SiloIdentityMode::SamlJit,
    )
    .await;

    // Create a user in advance
    create_jit_user(datastore, &silo, "external@id.com").await;

    let authn_opctx = nexus.opctx_external_authn();

    let (authz_silo, db_silo) =
        LookupPath::new(&authn_opctx, &nexus.datastore())
            .silo_name(&silo.identity.name.into())
            .fetch()
            .await
            .unwrap();

    // Should create two groups from the authenticated subject
    let existing_silo_user = nexus
        .silo_user_from_authenticated_subject(
            &authn_opctx,
            &authz_silo,
            &db_silo,
            &AuthenticatedSubject {
                external_id: "external@id.com".into(),
                groups: vec!["a-group".into(), "b-group".into()],
            },
        )
        .await
        .unwrap()
        .unwrap();

    let group_memberships = nexus
        .datastore()
        .silo_group_membership_for_user(
            &authn_opctx,
            &authz_silo,
            existing_silo_user.id(),
        )
        .await
        .unwrap();

    assert_eq!(group_memberships.len(), 2);

    let mut group_names = vec![];

    for group_membership in &group_memberships {
        let (.., db_group) = LookupPath::new(&authn_opctx, nexus.datastore())
            .silo_group_id(group_membership.silo_group_id)
            .fetch()
            .await
            .unwrap();

        group_names.push(db_group.external_id);
    }

    assert!(group_names.contains(&"a-group".to_string()));
    assert!(group_names.contains(&"b-group".to_string()));
}

#[nexus_test]
async fn test_silo_groups_fixed(cptestctx: &ControlPlaneTestContext) {
    let client = &cptestctx.external_client;
    let nexus = &cptestctx.server.apictx().nexus;

    let silo = create_silo(
        &client,
        "test-silo",
        true,
        shared::SiloIdentityMode::LocalOnly,
    )
    .await;

    // Create a user in advance
    create_local_user(
        client,
        &silo,
        &"external-id-com".parse().unwrap(),
        params::UserPassword::InvalidPassword,
    )
    .await;

    let authn_opctx = nexus.opctx_external_authn();

    let (authz_silo, db_silo) =
        LookupPath::new(&authn_opctx, &nexus.datastore())
            .silo_name(&silo.identity.name.into())
            .fetch()
            .await
            .unwrap();

    // Should not create groups from the authenticated subject
    let existing_silo_user = nexus
        .silo_user_from_authenticated_subject(
            &authn_opctx,
            &authz_silo,
            &db_silo,
            &AuthenticatedSubject {
                external_id: "external-id-com".into(),
                groups: vec!["a-group".into(), "b-group".into()],
            },
        )
        .await
        .unwrap()
        .unwrap();

    let group_memberships = nexus
        .datastore()
        .silo_group_membership_for_user(
            &authn_opctx,
            &authz_silo,
            existing_silo_user.id(),
        )
        .await
        .unwrap();

    assert_eq!(group_memberships.len(), 0);
}

#[nexus_test]
async fn test_silo_groups_remove_from_one_group(
    cptestctx: &ControlPlaneTestContext,
) {
    let client = &cptestctx.external_client;
    let nexus = &cptestctx.server.apictx().nexus;
    let datastore = nexus.datastore();

    let silo = create_silo(
        &client,
        "test-silo",
        true,
        shared::SiloIdentityMode::SamlJit,
    )
    .await;

    // Create a user in advance
    create_jit_user(datastore, &silo, "external@id.com").await;

    let authn_opctx = nexus.opctx_external_authn();

    let (authz_silo, db_silo) =
        LookupPath::new(&authn_opctx, &nexus.datastore())
            .silo_name(&silo.identity.name.into())
            .fetch()
            .await
            .unwrap();

    // Add to two groups
    let existing_silo_user = nexus
        .silo_user_from_authenticated_subject(
            &authn_opctx,
            &authz_silo,
            &db_silo,
            &AuthenticatedSubject {
                external_id: "external@id.com".into(),
                groups: vec!["a-group".into(), "b-group".into()],
            },
        )
        .await
        .unwrap()
        .unwrap();

    // Check those groups were created and the user was added
    let group_memberships = nexus
        .datastore()
        .silo_group_membership_for_user(
            &authn_opctx,
            &authz_silo,
            existing_silo_user.id(),
        )
        .await
        .unwrap();

    assert_eq!(group_memberships.len(), 2);

    let mut group_names = vec![];

    for group_membership in &group_memberships {
        let (.., db_group) = LookupPath::new(&authn_opctx, nexus.datastore())
            .silo_group_id(group_membership.silo_group_id)
            .fetch()
            .await
            .unwrap();

        group_names.push(db_group.external_id);
    }

    assert!(group_names.contains(&"a-group".to_string()));
    assert!(group_names.contains(&"b-group".to_string()));

    // Then remove their membership from one group
    let existing_silo_user = nexus
        .silo_user_from_authenticated_subject(
            &authn_opctx,
            &authz_silo,
            &db_silo,
            &AuthenticatedSubject {
                external_id: "external@id.com".into(),
                groups: vec!["b-group".into()],
            },
        )
        .await
        .unwrap()
        .unwrap();

    let group_memberships = nexus
        .datastore()
        .silo_group_membership_for_user(
            &authn_opctx,
            &authz_silo,
            existing_silo_user.id(),
        )
        .await
        .unwrap();

    assert_eq!(group_memberships.len(), 1);

    let mut group_names = vec![];

    for group_membership in &group_memberships {
        let (.., db_group) = LookupPath::new(&authn_opctx, nexus.datastore())
            .silo_group_id(group_membership.silo_group_id)
            .fetch()
            .await
            .unwrap();

        group_names.push(db_group.external_id);
    }

    assert!(group_names.contains(&"b-group".to_string()));
}

#[nexus_test]
async fn test_silo_groups_remove_from_both_groups(
    cptestctx: &ControlPlaneTestContext,
) {
    let client = &cptestctx.external_client;
    let nexus = &cptestctx.server.apictx().nexus;
    let datastore = nexus.datastore();

    let silo = create_silo(
        &client,
        "test-silo",
        true,
        shared::SiloIdentityMode::SamlJit,
    )
    .await;

    // Create a user in advance
    create_jit_user(datastore, &silo, "external@id.com").await;

    let authn_opctx = nexus.opctx_external_authn();

    let (authz_silo, db_silo) =
        LookupPath::new(&authn_opctx, &nexus.datastore())
            .silo_name(&silo.identity.name.into())
            .fetch()
            .await
            .unwrap();

    // Add to two groups
    let existing_silo_user = nexus
        .silo_user_from_authenticated_subject(
            &authn_opctx,
            &authz_silo,
            &db_silo,
            &AuthenticatedSubject {
                external_id: "external@id.com".into(),
                groups: vec!["a-group".into(), "b-group".into()],
            },
        )
        .await
        .unwrap()
        .unwrap();

    // Check those groups were created and the user was added
    let group_memberships = nexus
        .datastore()
        .silo_group_membership_for_user(
            &authn_opctx,
            &authz_silo,
            existing_silo_user.id(),
        )
        .await
        .unwrap();

    assert_eq!(group_memberships.len(), 2);

    let mut group_names = vec![];

    for group_membership in &group_memberships {
        let (.., db_group) = LookupPath::new(&authn_opctx, nexus.datastore())
            .silo_group_id(group_membership.silo_group_id)
            .fetch()
            .await
            .unwrap();

        group_names.push(db_group.external_id);
    }

    assert!(group_names.contains(&"a-group".to_string()));
    assert!(group_names.contains(&"b-group".to_string()));

    // Then remove from both groups, and add to a new one
    let existing_silo_user = nexus
        .silo_user_from_authenticated_subject(
            &authn_opctx,
            &authz_silo,
            &db_silo,
            &AuthenticatedSubject {
                external_id: "external@id.com".into(),
                groups: vec!["c-group".into()],
            },
        )
        .await
        .unwrap()
        .unwrap();

    let group_memberships = nexus
        .datastore()
        .silo_group_membership_for_user(
            &authn_opctx,
            &authz_silo,
            existing_silo_user.id(),
        )
        .await
        .unwrap();

    assert_eq!(group_memberships.len(), 1);

    let mut group_names = vec![];

    for group_membership in &group_memberships {
        let (.., db_group) = LookupPath::new(&authn_opctx, nexus.datastore())
            .silo_group_id(group_membership.silo_group_id)
            .fetch()
            .await
            .unwrap();

        group_names.push(db_group.external_id);
    }

    assert!(group_names.contains(&"c-group".to_string()));
}

// Test that silo delete cleans up associated groups
#[nexus_test]
async fn test_silo_delete_clean_up_groups(cptestctx: &ControlPlaneTestContext) {
    let client = &cptestctx.external_client;
    let nexus = &cptestctx.server.apictx().nexus;

    // Create a silo
    let silo = create_silo(
        &client,
        "test-silo",
        true,
        shared::SiloIdentityMode::SamlJit,
    )
    .await;

    let opctx_external_authn = nexus.opctx_external_authn();
    let opctx = OpContext::for_tests(
        cptestctx.logctx.log.new(o!()),
        nexus.datastore().clone(),
    );

    let (authz_silo, db_silo) = LookupPath::new(&opctx, &nexus.datastore())
        .silo_name(&silo.identity.name.into())
        .fetch()
        .await
        .unwrap();

    // Add a user with a group membership
    let silo_user = nexus
        .silo_user_from_authenticated_subject(
            &opctx_external_authn,
            &authz_silo,
            &db_silo,
            &AuthenticatedSubject {
                external_id: "user@company.com".into(),
                groups: vec!["sre".into()],
            },
        )
        .await
        .expect("silo_user_from_authenticated_subject")
        .unwrap();

    // Delete the silo
    NexusRequest::object_delete(&client, &"/system/silos/test-silo")
        .authn_as(AuthnMode::PrivilegedUser)
        .execute()
        .await
        .expect("failed to make request");

    // Expect the group is gone
    assert!(nexus
        .datastore()
        .silo_group_optional_lookup(
            &opctx_external_authn,
            &authz_silo,
            "a-group".into(),
        )
        .await
        .expect("silo_group_optional_lookup")
        .is_none());

    // Expect the group membership is gone
    let memberships = nexus
        .datastore()
        .silo_group_membership_for_user(
            &opctx_external_authn,
            &authz_silo,
            silo_user.id(),
        )
        .await
        .expect("silo_group_membership_for_user");

    assert!(memberships.is_empty());

    // Expect the user is gone
    LookupPath::new(&opctx_external_authn, &nexus.datastore())
        .silo_user_id(silo_user.id())
        .fetch()
        .await
        .expect_err("user found");
}

// Test ensuring the same group from different users
#[nexus_test]
async fn test_ensure_same_silo_group(cptestctx: &ControlPlaneTestContext) {
    let client = &cptestctx.external_client;
    let nexus = &cptestctx.server.apictx().nexus;

    // Create a silo
    let silo = create_silo(
        &client,
        "test-silo",
        true,
        shared::SiloIdentityMode::SamlJit,
    )
    .await;

    let opctx = OpContext::for_tests(
        cptestctx.logctx.log.new(o!()),
        nexus.datastore().clone(),
    );

    let (authz_silo, db_silo) = LookupPath::new(&opctx, &nexus.datastore())
        .silo_name(&silo.identity.name.into())
        .fetch()
        .await
        .unwrap();

    // Add the first user with a group membership
    let _silo_user_1 = nexus
        .silo_user_from_authenticated_subject(
            &nexus.opctx_external_authn(),
            &authz_silo,
            &db_silo,
            &AuthenticatedSubject {
                external_id: "user1@company.com".into(),
                groups: vec!["sre".into()],
            },
        )
        .await
        .expect("silo_user_from_authenticated_subject 1")
        .unwrap();

    // Add the first user with a group membership
    let _silo_user_2 = nexus
        .silo_user_from_authenticated_subject(
            &nexus.opctx_external_authn(),
            &authz_silo,
            &db_silo,
            &AuthenticatedSubject {
                external_id: "user2@company.com".into(),
                groups: vec!["sre".into()],
            },
        )
        .await
        .expect("silo_user_from_authenticated_subject 2")
        .unwrap();

    // TODO-coverage were we intending to verify something here?
}

/// Tests the behavior of the per-Silo "list users" and "fetch user" endpoints.
///
/// We'll run the tests separately for both kinds of Silo.  The implementation
/// should be the same, but that's why we're verifying it.
#[nexus_test]
async fn test_silo_user_views(cptestctx: &ControlPlaneTestContext) {
    let client = &cptestctx.external_client;
    let datastore = cptestctx.server.apictx().nexus.datastore();

    // Create the two Silos.
    let silo1 =
        create_silo(client, "silo1", false, shared::SiloIdentityMode::SamlJit)
            .await;
    let silo2 = create_silo(
        client,
        "silo2",
        false,
        shared::SiloIdentityMode::LocalOnly,
    )
    .await;

    // Create two users in each Silo.  We need two so that we can verify that an
    // ordinary user can see a user other than themselves in each Silo.
    let silo1_user1 = create_jit_user(datastore, &silo1, "silo1-user1").await;
    let silo1_user1_id = silo1_user1.id;
    let silo1_user2 = create_jit_user(datastore, &silo1, "silo1-user2").await;
    let silo1_user2_id = silo1_user2.id;
    let mut silo1_expected_users = [silo1_user1.clone(), silo1_user2.clone()];
    silo1_expected_users.sort_by_key(|u| u.id);

    let silo2_user1 = create_local_user(
        client,
        &silo2,
        &"silo2-user1".parse().unwrap(),
        params::UserPassword::InvalidPassword,
    )
    .await;
    let silo2_user1_id = silo2_user1.id;
    let silo2_user2 = create_local_user(
        client,
        &silo2,
        &"silo2-user2".parse().unwrap(),
        params::UserPassword::InvalidPassword,
    )
    .await;
    let silo2_user2_id = silo2_user2.id;
    let mut silo2_expected_users = [silo2_user1.clone(), silo2_user2.clone()];
    silo2_expected_users.sort_by_key(|u| u.id);

    let users_by_id = {
        let mut users_by_id: BTreeMap<Uuid, &views::User> = BTreeMap::new();
        assert_eq!(users_by_id.insert(silo1_user1_id, &silo1_user1), None);
        assert_eq!(users_by_id.insert(silo1_user2_id, &silo1_user2), None);
        assert_eq!(users_by_id.insert(silo2_user1_id, &silo2_user1), None);
        assert_eq!(users_by_id.insert(silo2_user2_id, &silo2_user2), None);
        users_by_id
    };

    let users_by_name = users_by_id
        .values()
        .map(|user| (user.display_name.to_owned(), *user))
        .collect::<BTreeMap<_, _>>();

    // We'll run through a battery of tests:
    // - for each of our test silos
    //   - for all *five* users ("test-privileged", plus the two users that we
    //     created in each Silo)
    //     - test the "list" endpoint
    //     - for all five user ids
    //       - test the "view user" endpoint for that user id
    //
    // This exercises a lot of different behaviors:
    // - on success, the "list" and "view" endpoints always return the right
    //   contents
    // - on failure, the "list" and "view" endpoints always return the right
    //   status code and message for the failure mode
    // - that users can always list and fetch all users in their own Silo via
    //   /system/silos (/users is tested elsewhere)
    // - that users without privileges cannot list or fetch users in other Silos
    // - that users with privileges on another Silo can list and fetch users in
    //   that Silo
    // - that a user with id "foo" in Silo1 cannot be accessed by that id in
    //   Silo 2.  This case is easy to miss but would be very bad to get wrong!
    let all_callers = {
        std::iter::once(AuthnMode::PrivilegedUser)
            .chain(users_by_name.values().map(|v| AuthnMode::SiloUser(v.id)))
            .collect::<Vec<_>>()
    };

    struct TestSilo<'a> {
        silo: &'a views::Silo,
        expected_users: [views::User; 2],
    }

    let test_silo1 =
        TestSilo { silo: &silo1, expected_users: silo1_expected_users };
    let test_silo2 =
        TestSilo { silo: &silo2, expected_users: silo2_expected_users };

    let mut output = String::new();
    for test_silo in [test_silo1, test_silo2] {
        let silo_name = &test_silo.silo.identity().name;
        let silo_users_url =
            &format!("/system/silos/{}/users", test_silo.silo.identity().name);

        write!(&mut output, "SILO: {}\n", silo_name).unwrap();

        for calling_user in all_callers.iter() {
            let caller_label = match calling_user {
                AuthnMode::PrivilegedUser => "privileged",
                AuthnMode::SiloUser(silo_user_id) => {
                    let user = users_by_id.get(silo_user_id).unwrap();
                    &user.display_name
                }
                _ => unimplemented!(),
            };
            write!(&mut output, "    test user {}:\n", caller_label).unwrap();

            // Test the "list" endpoint.
            write!(&mut output, "        list = ").unwrap();
            let test_response = NexusRequest::new(RequestBuilder::new(
                client,
                Method::GET,
                &format!("{}/all", silo_users_url),
            ))
            .authn_as(calling_user.clone())
            .execute()
            .await
            .unwrap();
            write!(&mut output, "{}", test_response.status.as_str()).unwrap();

            // If this succeeded, it must have returned the expected users for
            // this Silo.
            if test_response.status == http::StatusCode::OK {
                let found_users = test_response
                    .parsed_body::<dropshot::ResultsPage<views::User>>()
                    .unwrap()
                    .items;
                assert_eq!(found_users, test_silo.expected_users);
            } else {
                let error = test_response
                    .parsed_body::<dropshot::HttpErrorResponseBody>()
                    .unwrap();
                write!(&mut output, " (message = {:?})", error.message)
                    .unwrap();
            }

            write!(&mut output, "\n").unwrap();

            // Test the "view" endpoint for each user in this Silo.
            for (_, user) in &users_by_name {
                let user_id = user.id;
                write!(&mut output, "        view {:?} = ", user.display_name)
                    .unwrap();
                let test_response = NexusRequest::new(RequestBuilder::new(
                    client,
                    Method::GET,
                    &format!("{}/id/{}", silo_users_url, user_id),
                ))
                .authn_as(calling_user.clone())
                .execute()
                .await
                .unwrap();
                write!(&mut output, "{}", test_response.status.as_str())
                    .unwrap();
                // If this succeeded, it must have returned the right user back.
                if test_response.status == http::StatusCode::OK {
                    let found_user =
                        test_response.parsed_body::<views::User>().unwrap();
                    assert_eq!(
                        found_user.silo_id,
                        test_silo.silo.identity().id
                    );
                    assert_eq!(found_user, **user);
                } else {
                    let error = test_response
                        .parsed_body::<dropshot::HttpErrorResponseBody>()
                        .unwrap();
                    // Strip the identifier out of the error message because the
                    // uuid changes each time.
                    let pattern = regex::Regex::new("\".*?\"").unwrap();
                    let message = pattern.replace_all(&error.message, "...");
                    write!(&mut output, " (message = {:?})", message).unwrap();
                }

                write!(&mut output, "\n").unwrap();
            }

            write!(&mut output, "\n").unwrap();
        }
    }

    expectorate::assert_contents(
        "tests/output/silo-user-views-output.txt",
        &output,
    );
}

/// Create a user in a SamlJit Silo for testing
///
/// For local-only Silos, use the real API (via `create_local_user()`).
async fn create_jit_user(
    datastore: &db::DataStore,
    silo: &views::Silo,
    external_id: &str,
) -> views::User {
    assert_eq!(silo.identity_mode, shared::SiloIdentityMode::SamlJit);
    let silo_id = silo.identity.id;
    let silo_user_id = Uuid::new_v4();
    let authz_silo =
        authz::Silo::new(authz::FLEET, silo_id, LookupType::ById(silo_id));
    let silo_user =
        db::model::SiloUser::new(silo_id, silo_user_id, external_id.to_owned());
    datastore
        .silo_user_create(&authz_silo, silo_user)
        .await
        .expect("failed to create user in SamlJit Silo")
        .1
        .into()
}

/// Tests that LocalOnly-specific endpoints are not available in SamlJit Silos
#[nexus_test]
async fn test_jit_silo_constraints(cptestctx: &ControlPlaneTestContext) {
    let client = &cptestctx.external_client;
    let nexus = &cptestctx.server.apictx().nexus;
    let datastore = nexus.datastore();
    let silo =
        create_silo(&client, "jit", true, shared::SiloIdentityMode::SamlJit)
            .await;

    // We need one initial user that would in principle have privileges to
    // create other users.
    let admin_username = "admin-user";
    let admin_user = create_jit_user(&datastore, &silo, admin_username).await;

    // Grant this user "admin" privileges on that Silo.
    grant_iam(
        client,
        "/system/silos/jit",
        SiloRole::Admin,
        admin_user.id,
        AuthnMode::PrivilegedUser,
    )
    .await;

    // Neither the "test-privileged" user nor this newly-created admin user
    // ought to be able to create a user via the Silo's local identity provider
    // (because that provider does not exist).
    for caller in
        [AuthnMode::PrivilegedUser, AuthnMode::SiloUser(admin_user.id)]
    {
        verify_local_idp_404(
            NexusRequest::expect_failure_with_body(
                client,
                StatusCode::NOT_FOUND,
                Method::POST,
                "/system/silos/jit/identity-providers/local/users",
                &params::UserCreate {
                    external_id: params::UserId::from_str("dummy").unwrap(),
                    password: params::UserPassword::InvalidPassword,
                },
            )
            .authn_as(caller),
        )
        .await;
    }

    // Now create another user, as might happen via JIT.
    let other_user_id =
        create_jit_user(datastore, &silo, "other-user").await.id;
    let user_url_delete = format!(
        "/system/silos/jit/identity-providers/local/users/{}",
        other_user_id
    );
    let user_url_set_password = format!(
        "/system/silos/jit/identity-providers/local/users/{}/set-password",
        other_user_id
    );

    // Neither the "test-privileged" user nor the Silo Admin ought to be able to
    // remove this user via the local identity provider, nor set the user's
    // password.
    let password = params::Password::from_str("dummy").unwrap();
    for caller in
        [AuthnMode::PrivilegedUser, AuthnMode::SiloUser(admin_user.id)]
    {
        verify_local_idp_404(
            NexusRequest::expect_failure(
                client,
                StatusCode::NOT_FOUND,
                Method::DELETE,
                &user_url_delete,
            )
            .authn_as(caller.clone()),
        )
        .await;

        verify_local_idp_404(
            NexusRequest::expect_failure_with_body(
                client,
                StatusCode::NOT_FOUND,
                Method::POST,
                &user_url_set_password,
                &params::UserPassword::Password(password.clone()),
            )
            .authn_as(caller.clone()),
        )
        .await;
    }

    // One should also not be able to log into this kind of Silo with a username
    // and password.
    verify_local_idp_404(NexusRequest::expect_failure_with_body(
        client,
        StatusCode::NOT_FOUND,
        Method::POST,
        "/login/jit/local",
        &params::UsernamePasswordCredentials {
            username: params::UserId::from_str(admin_username).unwrap(),
            password: password.clone(),
        },
    ))
    .await;

    // They should get the same error for a user that does not exist.
    verify_local_idp_404(NexusRequest::expect_failure_with_body(
        client,
        StatusCode::NOT_FOUND,
        Method::POST,
        "/login/jit/local",
        &params::UsernamePasswordCredentials {
            username: params::UserId::from_str("bogus").unwrap(),
            password: password.clone(),
        },
    ))
    .await;
}

async fn verify_local_idp_404(request: NexusRequest<'_>) {
    let error = request
        .execute()
        .await
        .unwrap()
        .parsed_body::<dropshot::HttpErrorResponseBody>()
        .unwrap();
    assert_eq!(
        error.message,
        "not found: identity-provider with name \"local\""
    );
}

/// Tests that SamlJit-specific endpoints are not available in LocalOnly Silos
#[nexus_test]
async fn test_local_silo_constraints(cptestctx: &ControlPlaneTestContext) {
    let client = &cptestctx.external_client;

    // Create a "LocalOnly" Silo with its own admin user.
    let silo = create_silo(
        &client,
        "fixed",
        true,
        shared::SiloIdentityMode::LocalOnly,
    )
    .await;
    let new_silo_user_id = create_local_user(
        client,
        &silo,
        &"admin-user".parse().unwrap(),
        params::UserPassword::InvalidPassword,
    )
    .await
    .id;
    grant_iam(
        client,
        "/system/silos/fixed",
        SiloRole::Admin,
        new_silo_user_id,
        AuthnMode::PrivilegedUser,
    )
    .await;

    // It's not allowed to create an identity provider in a LocalOnly Silo.
    let error: dropshot::HttpErrorResponseBody =
        NexusRequest::expect_failure_with_body(
            client,
            StatusCode::BAD_REQUEST,
            Method::POST,
            "/system/silos/fixed/identity-providers/saml",
            &params::SamlIdentityProviderCreate {
                identity: IdentityMetadataCreateParams {
                    name: "some-totally-real-saml-provider"
                        .to_string()
                        .parse()
                        .unwrap(),
                    description: "a demo provider".to_string(),
                },

                idp_metadata_source:
                    params::IdpMetadataSource::Base64EncodedXml {
                        data: base64::engine::general_purpose::STANDARD
                            .encode(SAML_IDP_DESCRIPTOR),
                    },

                idp_entity_id: "entity_id".to_string(),
                sp_client_id: "client_id".to_string(),
                acs_url: "http://acs".to_string(),
                slo_url: "http://slo".to_string(),
                technical_contact_email: "technical@fake".to_string(),

                signing_keypair: None,

                group_attribute_name: None,
            },
        )
        .authn_as(AuthnMode::SiloUser(new_silo_user_id))
        .execute()
        .await
        .unwrap()
        .parsed_body()
        .unwrap();

    assert_eq!(
        error.message,
        "cannot create identity providers in this kind of Silo"
    );

    // The SAML login endpoints should not work, either.
    let error: dropshot::HttpErrorResponseBody = NexusRequest::expect_failure(
        client,
        StatusCode::NOT_FOUND,
        Method::GET,
        "/login/fixed/saml/foo",
    )
    .execute()
    .await
    .unwrap()
    .parsed_body()
    .unwrap();
    assert_eq!(error.message, "not found: identity-provider with name \"foo\"");
    let error: dropshot::HttpErrorResponseBody = NexusRequest::expect_failure(
        client,
        StatusCode::NOT_FOUND,
        Method::POST,
        "/login/fixed/saml/foo",
    )
    .execute()
    .await
    .unwrap()
    .parsed_body()
    .unwrap();
    assert_eq!(error.message, "not found: identity-provider with name \"foo\"");
}

#[nexus_test]
async fn test_local_silo_users(cptestctx: &ControlPlaneTestContext) {
    let client = &cptestctx.external_client;

    // Create a "LocalOnly" Silo for testing.
    let silo1 = create_silo(
        &client,
        "silo1",
        true,
        shared::SiloIdentityMode::LocalOnly,
    )
    .await;

    // We'll run through a battery of tests as each of two different users: the
    // usual "test-privileged" user (which should have full access because
    // they're a Fleet Administrator) as well as a newly-created Silo Admin
    // user.
    run_user_tests(client, &silo1, &AuthnMode::PrivilegedUser, &[]).await;

    // Create a Silo Admin in our test Silo and run through the same tests.
    let admin_user = create_local_user(
        client,
        &silo1,
        &"admin-user".parse().unwrap(),
        params::UserPassword::InvalidPassword,
    )
    .await;
    grant_iam(
        client,
        "/system/silos/silo1",
        SiloRole::Admin,
        admin_user.id,
        AuthnMode::PrivilegedUser,
    )
    .await;
    run_user_tests(
        client,
        &silo1,
        &AuthnMode::SiloUser(admin_user.id),
        &[admin_user.clone()],
    )
    .await;
}

/// Runs a sequence of tests for create, read, and delete of API-managed users
async fn run_user_tests(
    client: &dropshot::test_util::ClientTestContext,
    silo: &views::Silo,
    authn_mode: &AuthnMode,
    existing_users: &[views::User],
) {
    let url_all_users =
        format!("/system/silos/{}/users/all", silo.identity.name);
    let url_local_idp_users = format!(
        "/system/silos/{}/identity-providers/local/users",
        silo.identity.name
    );
    let url_user_create = url_local_idp_users.to_string();

    // Fetch users and verify it matches what the caller expects.
    println!("run_user_tests: as {:?}: fetch all users", authn_mode);
    let users = NexusRequest::object_get(client, &url_all_users)
        .authn_as(authn_mode.clone())
        .execute()
        .await
        .expect("failed to list users")
        .parsed_body::<dropshot::ResultsPage<views::User>>()
        .unwrap()
        .items;
    println!("users: {:?}", users);
    assert_eq!(users, existing_users);

    // Create a user.
    let user_created = NexusRequest::objects_post(
        client,
        &url_user_create,
        &params::UserCreate {
            external_id: params::UserId::from_str("a-test-user").unwrap(),
            password: params::UserPassword::InvalidPassword,
        },
    )
    .authn_as(authn_mode.clone())
    .execute()
    .await
    .expect("failed to create user")
    .parsed_body::<views::User>()
    .unwrap();
    assert_eq!(user_created.display_name, "a-test-user");
    println!("created user: {:?}", user_created);

    // Fetch the user we just created.
    let user_url_get = format!(
        "/system/silos/{}/users/id/{}",
        silo.identity.name, user_created.id
    );
    let user_found = NexusRequest::object_get(client, &user_url_get)
        .authn_as(authn_mode.clone())
        .execute()
        .await
        .expect("failed to fetch user we just created")
        .parsed_body::<views::User>()
        .unwrap();
    assert_eq!(user_created, user_found);

    // List users.  We should find whatever was there before, plus our new one.
    let new_users = NexusRequest::object_get(client, &url_all_users)
        .authn_as(authn_mode.clone())
        .execute()
        .await
        .expect("failed to list users")
        .parsed_body::<dropshot::ResultsPage<views::User>>()
        .unwrap()
        .items;
    println!("new_users: {:?}", new_users);
    let new_users = new_users
        .iter()
        .filter(|new_user| !users.iter().any(|old_user| *new_user == old_user))
        .collect::<Vec<_>>();
    assert_eq!(new_users, &[&user_created]);

    // Delete the user that we created.
    let user_url_delete = format!(
        "/system/silos/{}/identity-providers/local/users/{}",
        silo.identity.name, user_created.id
    );
    NexusRequest::object_delete(client, &user_url_delete)
        .authn_as(authn_mode.clone())
        .execute()
        .await
        .expect("failed to delete the user we just created");

    // We should not be able to fetch or delete the user again.
    for method in [Method::GET, Method::DELETE] {
        let url = if method == Method::GET {
            &user_url_get
        } else {
            &user_url_delete
        };
        let error = NexusRequest::expect_failure(
            client,
            StatusCode::NOT_FOUND,
            method,
            url,
        )
        .authn_as(authn_mode.clone())
        .execute()
        .await
        .expect("unexpectedly succeeded in fetching deleted user")
        .parsed_body::<dropshot::HttpErrorResponseBody>()
        .unwrap();
        let not_found_message =
            format!("not found: silo-user with id \"{}\"", user_created.id);
        assert_eq!(error.message, not_found_message);
    }

    // List users again.  We should just find whatever we started with.
    let last_users = NexusRequest::object_get(client, &url_all_users)
        .authn_as(authn_mode.clone())
        .execute()
        .await
        .expect("failed to list users")
        .parsed_body::<dropshot::ResultsPage<views::User>>()
        .unwrap()
        .items;
    println!("last_users: {:?}", last_users);
    assert_eq!(last_users, existing_users);
}
