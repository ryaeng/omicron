/*!
 * API simulation of an Oxide rack, used for testing and prototyping.
 */

use async_trait::async_trait;
use chrono::Utc;
use futures::lock::Mutex;
use futures::stream::StreamExt;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use crate::api_error::ApiError;
use crate::api_model::ApiBackend;
use crate::api_model::ApiIdentityMetadata;
use crate::api_model::ApiProject;
use crate::api_model::ApiProjectCreateParams;
use crate::api_model::ApiProjectUpdateParams;
use crate::api_model::ApiResourceType;
use crate::api_model::CreateResult;
use crate::api_model::DeleteResult;
use crate::api_model::ListResult;
use crate::api_model::LookupResult;
use crate::api_model::UpdateResult;

/**
 * SimulatorBuilder is used to initialize and populate a Simulator
 * synchronously, before we've wrapped the guts in a Mutex that can only be
 * locked in an async context.
 */
pub struct SimulatorBuilder {
    projects_by_name: BTreeSet<String>,
}

impl SimulatorBuilder {
    pub fn new() -> Self {
        SimulatorBuilder {
            projects_by_name: BTreeSet::new(),
        }
    }

    /**
     * Seed the simulator with a generic-looking project called "project_name".
     */
    pub fn project_create(&mut self, project_name: &str) {
        let name = project_name.to_string();
        self.projects_by_name.insert(name.clone());
    }

    /**
     * Return a Simulator instance holding the state created using this builder.
     */
    pub fn build(self) -> Simulator {
        let mut projects_by_name: BTreeMap<String, Arc<ApiProject>> =
            BTreeMap::new();
        let now = Utc::now();

        for projname in self.projects_by_name {
            let simproject = SimProject {};
            projects_by_name.insert(
                projname.clone(),
                Arc::new(ApiProject {
                    backend_impl: Box::new(simproject),
                    identity: ApiIdentityMetadata {
                        id: projname.clone(),
                        name: projname.clone(),
                        description: "<auto-generated at server startup>"
                            .to_string(),
                        time_created: now.clone(),
                        time_modified: now.clone(),
                    },
                    generation: 1,
                }),
            );
        }

        Simulator {
            projects_by_name: Arc::new(Mutex::new(projects_by_name)),
        }
    }
}

/**
 * Maintains simulated state of the Oxide rack.  The current implementation is
 * in-memory only.
 */
pub struct Simulator {
    /** all projects, indexed by name. */
    projects_by_name: Arc<Mutex<BTreeMap<String, Arc<ApiProject>>>>,
}

/**
 * Backend-specific implementation of an ApiProject.  We currently don't need
 * any additional fields for this.
 */
struct SimProject {}

#[async_trait]
impl ApiBackend for Simulator {
    async fn project_create(
        &self,
        new_project: &ApiProjectCreateParams,
    ) -> CreateResult<ApiProject> {
        let mut projects_by_name = self.projects_by_name.lock().await;
        if projects_by_name.contains_key(&new_project.identity.name) {
            return Err(ApiError::ObjectAlreadyExists {
                type_name: ApiResourceType::Project,
                object_name: new_project.identity.name.clone(),
            });
        }

        let now = Utc::now();
        let newname = &new_project.identity.name;
        let simproject = SimProject {};
        let project = Arc::new(ApiProject {
            backend_impl: Box::new(simproject),
            identity: ApiIdentityMetadata {
                id: newname.clone(),
                name: newname.clone(),
                description: new_project.identity.description.clone(),
                time_created: now.clone(),
                time_modified: now.clone(),
            },
            generation: 1,
        });

        let rv = Arc::clone(&project);
        projects_by_name.insert(newname.clone(), project);
        Ok(rv)
    }

    async fn projects_list(
        &self,
        marker: Option<String>,
        limit: usize,
    ) -> ListResult<ApiProject> {
        let projects_by_name = self.projects_by_name.lock().await;

        /*
         * We assemble the list of projects that we're going to return now,
         * under the lock, so that we can release the lock right away.  (This
         * also makes the lifetime of the return value far easier.)
         */
        let collect_projects =
            |iter: &mut dyn Iterator<Item = (&String, &Arc<ApiProject>)>| {
                iter.take(limit)
                    .map(|(_, arcproject)| Ok(Arc::clone(&arcproject)))
                    .collect::<Vec<Result<Arc<ApiProject>, ApiError>>>()
            };

        let projects = match marker {
            None => collect_projects(&mut projects_by_name.iter()),
            /*
             * NOTE: This range is inclusive on the low end because that
             * makes it easier for the client to know that it hasn't missed
             * some items in the namespace.  This does mean that clients
             * have to know to skip the first item on each page because
             * it'll be the same as the last item on the previous page.
             * TODO-cleanup would it be a problem to just make this an
             * exclusive bound?  It seems like you couldn't fail to see any
             * items that were present for the whole scan, which seems like
             * the main constraint.
             */
            Some(start_value) => {
                collect_projects(&mut projects_by_name.range(start_value..))
            }
        };

        Ok(futures::stream::iter(projects).boxed())
    }

    async fn project_lookup(&self, name: &String) -> LookupResult<ApiProject> {
        let projects = self.projects_by_name.lock().await;
        let project =
            projects.get(name).ok_or_else(|| ApiError::ObjectNotFound {
                type_name: ApiResourceType::Project,
                object_name: name.clone(),
            })?;
        let rv = Arc::clone(project);
        Ok(rv)
    }

    async fn project_delete(&self, name: &String) -> DeleteResult {
        let mut projects = self.projects_by_name.lock().await;
        projects.remove(name).ok_or_else(|| ApiError::ObjectNotFound {
            type_name: ApiResourceType::Project,
            object_name: name.clone(),
        })?;
        Ok(())
    }

    async fn project_update(
        &self,
        name: &String,
        new_params: &ApiProjectUpdateParams,
    ) -> UpdateResult<ApiProject> {
        let now = Utc::now();
        let mut projects = self.projects_by_name.lock().await;

        let oldproject: Arc<ApiProject> =
            projects.remove(name).ok_or_else(|| ApiError::ObjectNotFound {
                type_name: ApiResourceType::Project,
                object_name: name.clone(),
            })?;
        let newname = &new_params
            .identity
            .name
            .as_ref()
            .unwrap_or(&oldproject.identity.name);
        let newdescription = &new_params
            .identity
            .description
            .as_ref()
            .unwrap_or(&oldproject.identity.description);
        let newid = oldproject.identity.id.clone();
        let newgen = oldproject.generation + 1;

        /*
         * Right now, it's fine to just create a new backend object as we do
         * here.  It's not clear if that will work once we flesh out this
         * implementation (e.g., if there's other state in that object).
         * However, there could be other holders of the Arc<ApiProject> right
         * now, so we can't just move "backend_impl" into the new object.  We'd
         * have to either mutate the original ApiProject, construct a whole new
         * SimProject (which is what we do here because it's trivial), or else
         * put the SimProject behind an Arc.  (It's not clear that will make
         * sense -- the two ApiProjects will have different state!)
         */
        let beimpl: Box<SimProject> = Box::new(SimProject {});
        let newvalue = Arc::new(ApiProject {
            backend_impl: beimpl,
            identity: ApiIdentityMetadata {
                id: newid,
                name: (*newname).clone(),
                description: (*newdescription).clone(),
                time_created: oldproject.identity.time_created.clone(),
                time_modified: now.clone(),
            },
            generation: newgen,
        });

        let rv = Arc::clone(&newvalue);
        projects.insert(newvalue.identity.name.clone(), newvalue);
        Ok(rv)
    }
}
