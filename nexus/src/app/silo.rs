// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Silos, Users, and SSH Keys.

use crate::authz::ApiResource;
use crate::context::OpContext;
use crate::db;
use crate::db::identity::{Asset, Resource};
use crate::db::lookup::LookupPath;
use crate::db::model::Name;
use crate::db::model::SshKey;
use crate::external_api::params;
use crate::external_api::shared;
use crate::{authn, authz};
use anyhow::Context;
use nexus_db_model::UserProvisionType;
use omicron_common::api::external::http_pagination::PaginatedBy;
use omicron_common::api::external::DeleteResult;
use omicron_common::api::external::Error;
use omicron_common::api::external::ListResultVec;
use omicron_common::api::external::LookupResult;
use omicron_common::api::external::UpdateResult;
use omicron_common::api::external::{CreateResult, LookupType};
use omicron_common::api::external::{DataPageParams, ResourceType};
use omicron_common::bail_unless;
use std::str::FromStr;
use uuid::Uuid;

impl super::Nexus {
    // Silos

    pub async fn silo_create(
        &self,
        opctx: &OpContext,
        new_silo_params: params::SiloCreate,
    ) -> CreateResult<db::model::Silo> {
        // Silo group creation happens as Nexus's "external authn" context,
        // not the user's context here.  The user may not have permission to
        // create arbitrary groups in the Silo, but we allow them to create
        // this one in this case.
        let external_authn_opctx = self.opctx_external_authn();
        self.datastore()
            .silo_create(&opctx, &external_authn_opctx, new_silo_params)
            .await
    }

    pub async fn silos_list(
        &self,
        opctx: &OpContext,
        pagparams: &PaginatedBy<'_>,
    ) -> ListResultVec<db::model::Silo> {
        self.db_datastore.silos_list(opctx, pagparams).await
    }

    pub async fn silo_fetch(
        &self,
        opctx: &OpContext,
        name: &Name,
    ) -> LookupResult<db::model::Silo> {
        let (.., db_silo) = LookupPath::new(opctx, &self.db_datastore)
            .silo_name(name)
            .fetch()
            .await?;
        Ok(db_silo)
    }

    pub async fn silo_fetch_by_id(
        &self,
        opctx: &OpContext,
        silo_id: &Uuid,
    ) -> LookupResult<db::model::Silo> {
        let (.., db_silo) = LookupPath::new(opctx, &self.db_datastore)
            .silo_id(*silo_id)
            .fetch()
            .await?;
        Ok(db_silo)
    }

    pub async fn silo_delete(
        &self,
        opctx: &OpContext,
        name: &Name,
    ) -> DeleteResult {
        let (.., authz_silo, db_silo) =
            LookupPath::new(opctx, &self.db_datastore)
                .silo_name(name)
                .fetch_for(authz::Action::Delete)
                .await?;
        self.db_datastore.silo_delete(opctx, &authz_silo, &db_silo).await
    }

    // Role assignments

    pub async fn silo_fetch_policy(
        &self,
        opctx: &OpContext,
        silo_lookup: db::lookup::Silo<'_>,
    ) -> LookupResult<shared::Policy<authz::SiloRole>> {
        let (.., authz_silo) =
            silo_lookup.lookup_for(authz::Action::ReadPolicy).await?;
        let role_assignments = self
            .db_datastore
            .role_assignment_fetch_visible(opctx, &authz_silo)
            .await?
            .into_iter()
            .map(|r| r.try_into().context("parsing database role assignment"))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| Error::internal_error(&format!("{:#}", error)))?;
        Ok(shared::Policy { role_assignments })
    }

    pub async fn silo_update_policy(
        &self,
        opctx: &OpContext,
        silo_lookup: db::lookup::Silo<'_>,
        policy: &shared::Policy<authz::SiloRole>,
    ) -> UpdateResult<shared::Policy<authz::SiloRole>> {
        let (.., authz_silo) =
            silo_lookup.lookup_for(authz::Action::ModifyPolicy).await?;

        let role_assignments = self
            .db_datastore
            .role_assignment_replace_visible(
                opctx,
                &authz_silo,
                &policy.role_assignments,
            )
            .await?
            .into_iter()
            .map(|r| r.try_into())
            .collect::<Result<Vec<_>, _>>()?;

        Ok(shared::Policy { role_assignments })
    }

    // Users

    /// Helper function for looking up a user in a Silo
    ///
    /// `LookupPath` lets you look up users directly, regardless of what Silo
    /// they're in.  This helper validates that they're in the expected Silo.
    async fn silo_user_lookup_by_id(
        &self,
        opctx: &OpContext,
        authz_silo: &authz::Silo,
        silo_user_id: Uuid,
        action: authz::Action,
    ) -> LookupResult<(authz::SiloUser, db::model::SiloUser)> {
        let (_, authz_silo_user, db_silo_user) =
            LookupPath::new(opctx, self.datastore())
                .silo_user_id(silo_user_id)
                .fetch_for(action)
                .await?;
        if db_silo_user.silo_id != authz_silo.id() {
            return Err(authz_silo_user.not_found());
        }

        Ok((authz_silo_user, db_silo_user))
    }

    /// List the users in a Silo
    pub async fn silo_list_users(
        &self,
        opctx: &OpContext,
        silo_name: &Name,
        pagparams: &DataPageParams<'_, Uuid>,
    ) -> ListResultVec<db::model::SiloUser> {
        let (authz_silo,) = LookupPath::new(opctx, self.datastore())
            .silo_name(silo_name)
            .lookup_for(authz::Action::Read)
            .await?;
        let authz_silo_user_list = authz::SiloUserList::new(authz_silo);
        self.db_datastore
            .silo_users_list_by_id(opctx, &authz_silo_user_list, pagparams)
            .await
    }

    /// Fetch a user in a Silo
    pub async fn silo_user_fetch(
        &self,
        opctx: &OpContext,
        silo_name: &Name,
        silo_user_id: Uuid,
    ) -> LookupResult<db::model::SiloUser> {
        let (authz_silo,) = LookupPath::new(opctx, self.datastore())
            .silo_name(silo_name)
            .lookup_for(authz::Action::Read)
            .await?;
        let (_, db_silo_user) = self
            .silo_user_lookup_by_id(
                opctx,
                &authz_silo,
                silo_user_id,
                authz::Action::Read,
            )
            .await?;
        Ok(db_silo_user)
    }

    // The "local" identity provider (available only in `LocalOnly` Silos)

    /// Helper function for looking up a LocalOnly Silo by name
    ///
    /// This is called from contexts that are trying to access the "local"
    /// identity provider.  On failure, it returns a 404 for that identity
    /// provider.
    async fn local_idp_fetch_silo(
        &self,
        opctx: &OpContext,
        silo_name: &Name,
    ) -> LookupResult<(authz::Silo, db::model::Silo)> {
        let (authz_silo, db_silo) = LookupPath::new(opctx, &self.db_datastore)
            .silo_name(silo_name)
            .fetch()
            .await?;
        if db_silo.user_provision_type != UserProvisionType::ApiOnly {
            return Err(Error::not_found_by_name(
                ResourceType::IdentityProvider,
                &omicron_common::api::external::Name::from_str("local")
                    .unwrap(),
            ));
        }
        Ok((authz_silo, db_silo))
    }

    /// Create a user in a Silo's local identity provider
    pub async fn local_idp_create_user(
        &self,
        opctx: &OpContext,
        silo_name: &Name,
        new_user_params: params::UserCreate,
    ) -> CreateResult<db::model::SiloUser> {
        let (authz_silo, db_silo) =
            self.local_idp_fetch_silo(opctx, silo_name).await?;
        let authz_silo_user_list = authz::SiloUserList::new(authz_silo.clone());
        // TODO-cleanup This authz check belongs in silo_user_create().
        opctx
            .authorize(authz::Action::CreateChild, &authz_silo_user_list)
            .await?;
        let silo_user = db::model::SiloUser::new(
            authz_silo.id(),
            Uuid::new_v4(),
            new_user_params.external_id.as_ref().to_owned(),
        );
        // TODO These two steps should happen in a transaction.
        let (_, db_silo_user) =
            self.datastore().silo_user_create(&authz_silo, silo_user).await?;
        let authz_silo_user = authz::SiloUser::new(
            authz_silo.clone(),
            db_silo_user.id(),
            LookupType::ById(db_silo_user.id()),
        );
        self.silo_user_password_set_internal(
            opctx,
            &db_silo,
            &authz_silo_user,
            &db_silo_user,
            new_user_params.password,
        )
        .await?;
        Ok(db_silo_user)
    }

    /// Delete a user in a Silo's local identity provider
    pub async fn local_idp_delete_user(
        &self,
        opctx: &OpContext,
        silo_name: &Name,
        silo_user_id: Uuid,
    ) -> DeleteResult {
        let (authz_silo, _) =
            self.local_idp_fetch_silo(opctx, silo_name).await?;
        let (authz_silo_user, _) = self
            .silo_user_lookup_by_id(
                opctx,
                &authz_silo,
                silo_user_id,
                authz::Action::Delete,
            )
            .await?;
        self.db_datastore.silo_user_delete(opctx, &authz_silo_user).await
    }

    /// Based on an authenticated subject, fetch or create a silo user
    pub async fn silo_user_from_authenticated_subject(
        &self,
        opctx: &OpContext,
        authz_silo: &authz::Silo,
        db_silo: &db::model::Silo,
        authenticated_subject: &authn::silos::AuthenticatedSubject,
    ) -> LookupResult<Option<db::model::SiloUser>> {
        // XXX create user permission?
        opctx.authorize(authz::Action::CreateChild, authz_silo).await?;
        opctx.authorize(authz::Action::ListChildren, authz_silo).await?;

        let fetch_result = self
            .datastore()
            .silo_user_fetch_by_external_id(
                opctx,
                &authz_silo,
                &authenticated_subject.external_id,
            )
            .await?;

        let (authz_silo_user, db_silo_user) =
            if let Some(existing_silo_user) = fetch_result {
                existing_silo_user
            } else {
                // In this branch, no user exists for the authenticated subject
                // external id. The next action depends on the silo's user
                // provision type.
                match db_silo.user_provision_type {
                    // If the user provision type is ApiOnly, do not create a
                    // new user if one does not exist.
                    db::model::UserProvisionType::ApiOnly => {
                        return Ok(None);
                    }

                    // If the user provision type is JIT, then create the user if
                    // one does not exist
                    db::model::UserProvisionType::Jit => {
                        let silo_user = db::model::SiloUser::new(
                            authz_silo.id(),
                            Uuid::new_v4(),
                            authenticated_subject.external_id.clone(),
                        );

                        self.db_datastore
                            .silo_user_create(&authz_silo, silo_user)
                            .await?
                    }
                }
            };

        // Gather a list of groups that the user is part of based on what the
        // IdP sent us. Also, if the silo user provision type is Jit, create
        // silo groups if new groups from the IdP are seen.

        let mut silo_user_group_ids: Vec<Uuid> =
            Vec::with_capacity(authenticated_subject.groups.len());

        for group in &authenticated_subject.groups {
            let silo_group = match db_silo.user_provision_type {
                db::model::UserProvisionType::ApiOnly => {
                    self.db_datastore
                        .silo_group_optional_lookup(
                            opctx,
                            &authz_silo,
                            group.clone(),
                        )
                        .await?
                }

                db::model::UserProvisionType::Jit => {
                    let silo_group = self
                        .silo_group_lookup_or_create_by_name(
                            opctx,
                            &authz_silo,
                            &group,
                        )
                        .await?;

                    Some(silo_group)
                }
            };

            if let Some(silo_group) = silo_group {
                silo_user_group_ids.push(silo_group.id());
            }
        }

        // Update the user's group memberships

        self.db_datastore
            .silo_group_membership_replace_for_user(
                opctx,
                &authz_silo_user,
                silo_user_group_ids,
            )
            .await?;

        Ok(Some(db_silo_user))
    }

    // Silo user passwords

    /// Set or invalidate a Silo user's password
    ///
    /// If `password` is `UserPassword::Password`, the password is set to the
    /// requested value.  Otherwise, any existing password is invalidated so
    /// that it cannot be used for authentication any more.
    pub async fn local_idp_user_set_password(
        &self,
        opctx: &OpContext,
        silo_name: &Name,
        silo_user_id: Uuid,
        password_value: params::UserPassword,
    ) -> UpdateResult<()> {
        let (authz_silo, db_silo) =
            self.local_idp_fetch_silo(opctx, silo_name).await?;
        let (authz_silo_user, db_silo_user) = self
            .silo_user_lookup_by_id(
                opctx,
                &authz_silo,
                silo_user_id,
                authz::Action::Modify,
            )
            .await?;
        self.silo_user_password_set_internal(
            opctx,
            &db_silo,
            &authz_silo_user,
            &db_silo_user,
            password_value,
        )
        .await
    }

    /// Internal helper for setting a user's password
    ///
    /// The caller should have already verified that this is a `LocalOnly` Silo
    /// and that the specified user is in that Silo.
    async fn silo_user_password_set_internal(
        &self,
        opctx: &OpContext,
        db_silo: &db::model::Silo,
        authz_silo_user: &authz::SiloUser,
        db_silo_user: &db::model::SiloUser,
        password_value: params::UserPassword,
    ) -> UpdateResult<()> {
        let password_hash = match password_value {
            params::UserPassword::InvalidPassword => None,
            params::UserPassword::Password(password) => {
                let mut hasher = nexus_passwords::Hasher::default();
                let password_hash = hasher
                    .create_password(password.as_ref())
                    .map_err(|e| {
                    Error::internal_error(&format!("setting password: {:#}", e))
                })?;
                Some(db::model::SiloUserPasswordHash::new(
                    authz_silo_user.id(),
                    nexus_db_model::PasswordHashString::from(password_hash),
                ))
            }
        };

        self.datastore()
            .silo_user_password_hash_set(
                opctx,
                db_silo,
                authz_silo_user,
                db_silo_user,
                password_hash,
            )
            .await
    }

    /// Verify a Silo user's password
    ///
    /// To prevent timing attacks that would allow an attacker to learn the
    /// identity of a valid user, it's important that password verification take
    /// the same amount of time whether a user exists or not.  To achieve that,
    /// callers are expected to invoke this function during authentication even
    /// if they've found no user to match the requested credentials.  That's why
    /// this function accepts `Option<SiloUser>` rather than just a `SiloUser`.
    pub async fn silo_user_password_verify(
        &self,
        opctx: &OpContext,
        maybe_authz_silo_user: Option<&authz::SiloUser>,
        password: &nexus_passwords::Password,
    ) -> Result<bool, Error> {
        let maybe_hash = match maybe_authz_silo_user {
            None => None,
            Some(authz_silo_user) => {
                self.datastore()
                    .silo_user_password_hash_fetch(opctx, authz_silo_user)
                    .await?
            }
        };

        let mut hasher = nexus_passwords::Hasher::default();
        match maybe_hash {
            None => {
                // If the user or their password hash does not exist, create a
                // dummy password hash anyway.  This avoids exposing a timing
                // attack where an attacker can learn that a user exists by
                // seeing how fast it took a login attempt to fail.
                let _ = hasher.create_password(password);
                Ok(false)
            }
            Some(silo_user_password_hash) => Ok(hasher
                .verify_password(password, &silo_user_password_hash.hash)
                .map_err(|e| {
                    Error::internal_error(&format!(
                        "verifying password: {:#}",
                        e
                    ))
                })?),
        }
    }

    /// Given a silo name and username/password credentials, verify the
    /// credentials and return the corresponding SiloUser.
    pub async fn login_local(
        &self,
        opctx: &OpContext,
        silo_name: &Name,
        credentials: params::UsernamePasswordCredentials,
    ) -> Result<Option<db::model::SiloUser>, Error> {
        let (authz_silo, _) =
            self.local_idp_fetch_silo(opctx, silo_name).await?;

        // NOTE: It's very important that we not bail out early if we fail to
        // find a user with this external id.  See the note in
        // silo_user_password_verify().
        // TODO-security There may still be some vulnerability to timing attack
        // here, in that we'll do one fewer database lookup if a user does not
        // exist.  Rate limiting might help.  See omicron#2184.
        let fetch_user = self
            .datastore()
            .silo_user_fetch_by_external_id(
                opctx,
                &authz_silo,
                credentials.username.as_ref(),
            )
            .await?;
        let verified = self
            .silo_user_password_verify(
                opctx,
                fetch_user.as_ref().map(|(authz_silo_user, _)| authz_silo_user),
                credentials.password.as_ref(),
            )
            .await?;
        if verified {
            bail_unless!(
                fetch_user.is_some(),
                "passed password verification without a valid user"
            );
            let db_user = fetch_user.unwrap().1;
            Ok(Some(db_user))
        } else {
            Ok(None)
        }
    }

    // Silo groups

    pub async fn silo_group_lookup_or_create_by_name(
        &self,
        opctx: &OpContext,
        authz_silo: &authz::Silo,
        external_id: &String,
    ) -> LookupResult<db::model::SiloGroup> {
        match self
            .db_datastore
            .silo_group_optional_lookup(opctx, authz_silo, external_id.clone())
            .await?
        {
            Some(silo_group) => Ok(silo_group),

            None => {
                self.db_datastore
                    .silo_group_ensure(
                        opctx,
                        authz_silo,
                        db::model::SiloGroup::new(
                            Uuid::new_v4(),
                            authz_silo.id(),
                            external_id.clone(),
                        ),
                    )
                    .await
            }
        }
    }

    // SSH Keys

    pub async fn ssh_key_create(
        &self,
        opctx: &OpContext,
        silo_user_id: Uuid,
        params: params::SshKeyCreate,
    ) -> CreateResult<db::model::SshKey> {
        let ssh_key = db::model::SshKey::new(silo_user_id, params);
        let (.., authz_user) = LookupPath::new(opctx, &self.datastore())
            .silo_user_id(silo_user_id)
            .lookup_for(authz::Action::CreateChild)
            .await?;
        assert_eq!(authz_user.id(), silo_user_id);
        self.db_datastore.ssh_key_create(opctx, &authz_user, ssh_key).await
    }

    pub async fn ssh_keys_list(
        &self,
        opctx: &OpContext,
        silo_user_id: Uuid,
        page_params: &DataPageParams<'_, Name>,
    ) -> ListResultVec<SshKey> {
        let (.., authz_user) = LookupPath::new(opctx, &self.datastore())
            .silo_user_id(silo_user_id)
            .lookup_for(authz::Action::ListChildren)
            .await?;
        assert_eq!(authz_user.id(), silo_user_id);
        self.db_datastore.ssh_keys_list(opctx, &authz_user, page_params).await
    }

    pub async fn ssh_key_fetch(
        &self,
        opctx: &OpContext,
        silo_user_id: Uuid,
        ssh_key_name: &Name,
    ) -> LookupResult<SshKey> {
        let (.., ssh_key) = LookupPath::new(opctx, &self.datastore())
            .silo_user_id(silo_user_id)
            .ssh_key_name(ssh_key_name)
            .fetch()
            .await?;
        assert_eq!(ssh_key.name(), &ssh_key_name.0);
        Ok(ssh_key)
    }

    pub async fn ssh_key_delete(
        &self,
        opctx: &OpContext,
        silo_user_id: Uuid,
        ssh_key_name: &Name,
    ) -> DeleteResult {
        let (.., authz_user, authz_ssh_key) =
            LookupPath::new(opctx, &self.datastore())
                .silo_user_id(silo_user_id)
                .ssh_key_name(ssh_key_name)
                .lookup_for(authz::Action::Delete)
                .await?;
        assert_eq!(authz_user.id(), silo_user_id);
        self.db_datastore.ssh_key_delete(opctx, &authz_ssh_key).await
    }

    // identity providers

    pub async fn identity_provider_list(
        &self,
        opctx: &OpContext,
        silo_name: &Name,
        pagparams: &DataPageParams<'_, Name>,
    ) -> ListResultVec<db::model::IdentityProvider> {
        let (authz_silo, ..) = LookupPath::new(opctx, &self.db_datastore)
            .silo_name(silo_name)
            .fetch()
            .await?;
        let authz_idp_list = authz::SiloIdentityProviderList::new(authz_silo);
        self.db_datastore
            .identity_provider_list(opctx, &authz_idp_list, pagparams)
            .await
    }

    // Silo authn identity providers

    pub async fn saml_identity_provider_create(
        &self,
        opctx: &OpContext,
        silo_name: &Name,
        params: params::SamlIdentityProviderCreate,
    ) -> CreateResult<db::model::SamlIdentityProvider> {
        let (authz_silo, db_silo) = LookupPath::new(opctx, &self.db_datastore)
            .silo_name(silo_name)
            .fetch()
            .await?;
        let authz_idp_list = authz::SiloIdentityProviderList::new(authz_silo);

        if db_silo.user_provision_type != UserProvisionType::Jit {
            return Err(Error::invalid_request(
                "cannot create identity providers in this kind of Silo",
            ));
        }

        // This check is not strictly necessary yet.  We'll check this
        // permission in the DataStore when we actually update the list.
        // But we check now to protect the code that fetches the descriptor from
        // an external source.
        opctx.authorize(authz::Action::CreateChild, &authz_idp_list).await?;

        // The authentication mode is immutable so it's safe to check this here
        // and bail out.
        if db_silo.authentication_mode
            != nexus_db_model::AuthenticationMode::Saml
        {
            return Err(Error::invalid_request(&format!(
                "cannot create SAML identity provider for this Silo type \
                (expected authentication mode {:?}, found {:?})",
                nexus_db_model::AuthenticationMode::Saml,
                &db_silo.authentication_mode,
            )));
        }

        let idp_metadata_document_string = match &params.idp_metadata_source {
            params::IdpMetadataSource::Url { url } => {
                // Download the SAML IdP descriptor, and write it into the DB.
                // This is so that it can be deserialized later.
                //
                // Importantly, do this only once and store it. It would
                // introduce attack surface to download it each time it was
                // required.
                let dur = std::time::Duration::from_secs(5);
                let client = reqwest::ClientBuilder::new()
                    .connect_timeout(dur)
                    .timeout(dur)
                    .build()
                    .map_err(|e| {
                        Error::internal_error(&format!(
                            "failed to build reqwest client: {}",
                            e
                        ))
                    })?;

                let response = client.get(url).send().await.map_err(|e| {
                    Error::InvalidValue {
                        label: String::from("url"),
                        message: format!("error querying url: {}", e),
                    }
                })?;

                if !response.status().is_success() {
                    return Err(Error::InvalidValue {
                        label: String::from("url"),
                        message: format!(
                            "querying url returned: {}",
                            response.status()
                        ),
                    });
                }

                response.text().await.map_err(|e| Error::InvalidValue {
                    label: String::from("url"),
                    message: format!("error getting text from url: {}", e),
                })?
            }

            params::IdpMetadataSource::Base64EncodedXml { data } => {
                let bytes = base64::Engine::decode(
                    &base64::engine::general_purpose::STANDARD,
                    data,
                )
                .map_err(|e| Error::InvalidValue {
                    label: String::from("data"),
                    message: format!(
                        "error getting decoding base64 data: {}",
                        e
                    ),
                })?;
                String::from_utf8_lossy(&bytes).into_owned()
            }
        };

        let provider = db::model::SamlIdentityProvider {
            identity: db::model::SamlIdentityProviderIdentity::new(
                Uuid::new_v4(),
                params.identity,
            ),
            silo_id: db_silo.id(),

            idp_metadata_document_string,

            idp_entity_id: params.idp_entity_id,
            sp_client_id: params.sp_client_id,
            acs_url: params.acs_url,
            slo_url: params.slo_url,
            technical_contact_email: params.technical_contact_email,
            public_cert: params
                .signing_keypair
                .as_ref()
                .map(|x| x.public_cert.clone()),
            private_key: params
                .signing_keypair
                .as_ref()
                .map(|x| x.private_key.clone()),

            group_attribute_name: params.group_attribute_name.clone(),
        };

        let _authn_provider: authn::silos::SamlIdentityProvider =
            provider.clone().try_into().map_err(|e: anyhow::Error|
                // If an error is encountered converting from the model to the
                // authn type here, this is a request error: something about the
                // parameters of this request doesn't work.
                Error::invalid_request(&e.to_string()))?;

        self.db_datastore
            .saml_identity_provider_create(opctx, &authz_idp_list, provider)
            .await
    }

    pub async fn saml_identity_provider_fetch(
        &self,
        opctx: &OpContext,
        silo_name: &Name,
        provider_name: &Name,
    ) -> LookupResult<db::model::SamlIdentityProvider> {
        let (.., saml_identity_provider) =
            LookupPath::new(opctx, &self.datastore())
                .silo_name(silo_name)
                .saml_identity_provider_name(provider_name)
                .fetch()
                .await?;
        Ok(saml_identity_provider)
    }
}
