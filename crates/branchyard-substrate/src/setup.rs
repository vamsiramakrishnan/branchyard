//! Reconcile explicitly named atespaces and immutable bridge templates.

use branchyard_sandbox::ProviderError;
use serde::Serialize;
use tonic::{Code, Status};

use crate::actors::{resource_name, template_reference, Actors, Error};
use crate::provider::runtime;
use crate::template::{
    bridge_template_with, BridgeOptions, BridgeTemplate, BRIDGE_PORT, IDENTITY_DIR,
};
use crate::{pb, SubstrateProvider};

/// One resource that setup needs to create.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum SetupAction {
    /// Create an absent atespace.
    CreateAtespace {
        /// Atespace name.
        name: String,
    },
    /// Create the absent immutable template.
    CreateTemplate,
}

/// A reviewable setup operation. Applying it rechecks the observed resources.
#[derive(Clone, Debug, Serialize)]
pub struct SetupPlan {
    /// Atespace where task actors run.
    pub atespace: String,
    /// Qualified template reference.
    pub template: String,
    /// Pinned image used by the template.
    pub image: String,
    /// Resources absent when the plan was made; empty means already matching.
    pub actions: Vec<SetupAction>,
    #[serde(skip)]
    desired: pb::ActorTemplate,
    #[serde(skip)]
    observed_uid: Option<String>,
}

/// Progress of applying a setup plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupEvent {
    /// Rechecking resources before changes.
    Checking,
    /// Creating an absent atespace.
    CreatingAtespace(String),
    /// Creating the immutable actor template.
    CreatingTemplate,
    /// The named resources now match the plan.
    Ready,
}

/// Resources verified after setup finishes.
#[derive(Clone, Debug, Serialize)]
pub struct SetupResult {
    /// Actor atespace.
    pub atespace: String,
    /// Qualified template reference.
    pub template: String,
    /// Whether any missing atespace was created.
    pub created_atespace: bool,
    /// Whether the template was created.
    pub created_template: bool,
}

/// Authenticated template evidence, without a claim of running an actor.
#[derive(Clone, Debug, Serialize)]
pub struct TemplateInspection {
    /// Template atespace.
    pub atespace: String,
    /// Template name.
    pub name: String,
    /// Pinned bridge image.
    pub image: String,
    /// Bridge listener and wakeup probe port.
    pub bridge_port: u16,
    /// The template declares the bridge execution contract.
    pub exec: bool,
    /// Public verifying key embedded in the image's template environment.
    pub public_key: String,
}

pub(crate) const SIGNER_MISMATCH: &str =
    "the template's bridge verifying key does not match the configured signing key";

/// Why reading the configured template failed, before a caller picks its error type.
pub(crate) enum TemplateFailure {
    /// The `GetActorTemplate` call failed.
    Control(Error),
    /// Control returned a template this client cannot use.
    Unusable(ProviderError),
    /// The template's bridge verifying key is not the configured signer's.
    SignerMismatch,
}

impl SubstrateProvider {
    /// Read and validate the configured template through authenticated Control.
    pub fn inspect_template(&self) -> Result<TemplateInspection, ProviderError> {
        self.read_template().map_err(|failure| match failure {
            TemplateFailure::Control(error) => runtime(error),
            TemplateFailure::Unusable(error) => error,
            TemplateFailure::SignerMismatch => ProviderError::Invalid(SIGNER_MISMATCH.into()),
        })
    }

    /// One `GetActorTemplate` call, then the checks every inspection makes.
    pub(crate) fn read_template(&self) -> Result<TemplateInspection, TemplateFailure> {
        let template = self.config().template.clone();
        let found = self
            .run(async move |actors| actors.template(&template).await)
            .map_err(TemplateFailure::Control)?;
        let inspection = inspect(&found).map_err(TemplateFailure::Unusable)?;
        if self
            .config()
            .signer
            .as_ref()
            .is_some_and(|signer| signer.public_key() != inspection.public_key)
        {
            return Err(TemplateFailure::SignerMismatch);
        }
        Ok(inspection)
    }

    /// Inspect only the named resources and plan an immutable template setup.
    pub fn plan_setup(
        &self,
        spec: &BridgeTemplate,
        options: &BridgeOptions,
    ) -> Result<SetupPlan, ProviderError> {
        let reference = template_reference(&self.config().atespace, &self.config().template)
            .map_err(|e| ProviderError::Invalid(e.to_string()))?;
        if spec.name != reference.name {
            return Err(ProviderError::Invalid(
                "setup template name does not match the configured template".into(),
            ));
        }
        validate_spec(spec)?;
        if self
            .config()
            .signer
            .as_ref()
            .is_some_and(|signer| signer.public_key() != spec.public_key)
        {
            return Err(ProviderError::Invalid(
                "setup verifier does not match the configured signing key".into(),
            ));
        }
        let desired = bridge_template_with(&reference.atespace, spec, options);
        let atespace = self.config().atespace.clone();
        self.block(async move |actors| make_plan(actors, atespace, desired).await)
    }

    /// Re-observe a plan, create missing resources and refuse immutable conflicts.
    pub fn apply_setup(
        &self,
        plan: &SetupPlan,
        mut progress: impl FnMut(SetupEvent),
    ) -> Result<SetupResult, ProviderError> {
        let configured = template_reference(&self.config().atespace, &self.config().template)
            .map_err(|e| ProviderError::Invalid(e.to_string()))?;
        if plan.atespace != self.config().atespace || plan.template != qualified(&configured) {
            return Err(ProviderError::Invalid(
                "setup plan belongs to a different provider configuration".into(),
            ));
        }
        progress(SetupEvent::Checking);
        let plan = plan.clone();
        self.block(async move |actors| {
            let current = make_plan(actors, plan.atespace.clone(), plan.desired.clone()).await?;
            if plan.observed_uid.is_some() && plan.observed_uid != current.observed_uid {
                return Err(Status::failed_precondition(
                    "the template was replaced after planning; make a new setup plan",
                )
                .into());
            }
            let mut result = SetupResult {
                atespace: plan.atespace,
                template: plan.template,
                created_atespace: false,
                created_template: false,
            };
            for action in current.actions {
                match action {
                    SetupAction::CreateAtespace { name } => {
                        progress(SetupEvent::CreatingAtespace(name.clone()));
                        match actors
                            .client
                            .create_atespace(pb::CreateAtespaceRequest {
                                atespace: Some(pb::Atespace {
                                    metadata: Some(pb::ResourceMetadata {
                                        name,
                                        ..Default::default()
                                    }),
                                }),
                            })
                            .await
                        {
                            Ok(_) => result.created_atespace = true,
                            Err(error) if error.code() == Code::AlreadyExists => {}
                            Err(error) => return Err(error.into()),
                        }
                    }
                    SetupAction::CreateTemplate => {
                        progress(SetupEvent::CreatingTemplate);
                        match actors
                            .client
                            .create_actor_template(pb::CreateActorTemplateRequest {
                                actor_template: Some(plan.desired.clone()),
                            })
                            .await
                        {
                            Ok(_) => result.created_template = true,
                            Err(error) if error.code() == Code::AlreadyExists => {}
                            Err(error) => return Err(error.into()),
                        }
                    }
                }
            }
            let verified = make_plan(actors, result.atespace.clone(), plan.desired).await?;
            if !verified.actions.is_empty() {
                return Err(Status::failed_precondition(
                    "setup resources disappeared during apply; run setup again",
                )
                .into());
            }
            progress(SetupEvent::Ready);
            Ok(result)
        })
    }
}

async fn make_plan(
    actors: &mut Actors,
    atespace: String,
    desired: pb::ActorTemplate,
) -> Result<SetupPlan, Error> {
    let metadata = desired
        .metadata
        .as_ref()
        .ok_or(Error::MissingField("template.metadata"))?;
    let reference = pb::ObjectRef {
        atespace: metadata.atespace.clone(),
        name: metadata.name.clone(),
    };
    let mut namespaces = vec![atespace.clone()];
    if metadata.atespace != atespace {
        namespaces.push(metadata.atespace.clone());
    }
    let mut actions = Vec::new();
    for name in namespaces {
        match actors
            .client
            .get_atespace(pb::GetAtespaceRequest {
                atespace: Some(pb::ObjectRef {
                    atespace: String::new(),
                    name: name.clone(),
                }),
            })
            .await
        {
            Ok(_) => {}
            Err(error) if error.code() == Code::NotFound => {
                actions.push(SetupAction::CreateAtespace { name })
            }
            Err(error) => return Err(error.into()),
        }
    }
    let observed_uid = match actors.template(&qualified(&reference)).await {
        Ok(existing) => {
            if normalized(existing.clone()) != normalized(desired.clone()) {
                return Err(Status::failed_precondition("the immutable template differs from the requested image, verifier, command, volumes, pool or resources; choose a new template name").into());
            }
            existing.metadata.map(|metadata| metadata.uid)
        }
        Err(Error::Rpc(error)) if error.code() == Code::NotFound => {
            actions.push(SetupAction::CreateTemplate);
            None
        }
        Err(error) => return Err(error),
    };
    Ok(SetupPlan {
        atespace,
        template: qualified(&reference),
        image: desired
            .containers
            .first()
            .map(|c| c.image.clone())
            .unwrap_or_default(),
        actions,
        desired,
        observed_uid,
    })
}

fn qualified(reference: &pb::ObjectRef) -> String {
    format!("{}/{}", reference.atespace, reference.name)
}

fn normalized(mut template: pb::ActorTemplate) -> pb::ActorTemplate {
    if let Some(metadata) = &mut template.metadata {
        *metadata = pb::ResourceMetadata {
            atespace: metadata.atespace.clone(),
            name: metadata.name.clone(),
            ..Default::default()
        };
    }
    template.status = None;
    if template
        .resources
        .as_ref()
        .is_some_and(|r| r.limits.is_empty())
    {
        template.resources = None;
    }
    template
}

fn validate_spec(spec: &BridgeTemplate) -> Result<(), ProviderError> {
    let digest = spec.image.rsplit_once("@sha256:");
    if !digest.is_some_and(|(name, digest)| {
        !name.is_empty() && digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())
    }) {
        return Err(ProviderError::Invalid(
            "setup image must be pinned with a complete sha256 digest".into(),
        ));
    }
    if !resource_name(&spec.name)
        || !std::path::Path::new(&spec.bridge).is_absolute()
        || spec.sandbox_config.is_empty()
        || spec.storage_location.is_empty()
    {
        return Err(ProviderError::Invalid("setup needs a template name, absolute bridge path, SandboxConfig and snapshot storage URI".into()));
    }
    branchyard_bridge::Verifier::from_hex(&spec.public_key)?;
    Ok(())
}

fn inspect(template: &pb::ActorTemplate) -> Result<TemplateInspection, ProviderError> {
    let invalid = || {
        ProviderError::Invalid("template must run branchyard-bridge on port 80, probe /healthz and project atespace/name/uid identity".into())
    };
    let container = template
        .containers
        .iter()
        .find(|c| c.env.iter().any(|e| e.name == branchyard_bridge::KEY_ENV))
        .ok_or_else(invalid)?;
    let argument = |flag: &str| {
        container
            .command
            .windows(2)
            .find(|pair| pair[0] == flag)
            .map(|pair| pair[1].as_str())
    };
    if !container.command.iter().any(|arg| arg == "serve")
        || argument("--listen") != Some("0.0.0.0:80")
        || argument("--identity") != Some(IDENTITY_DIR)
    {
        return Err(invalid());
    }
    if argument("--run-as").is_some() {
        let capabilities = container
            .security_context
            .as_ref()
            .and_then(|context| context.capabilities.as_ref());
        if !capabilities.is_some_and(|caps| {
            ["SETUID", "SETGID"]
                .iter()
                .all(|required| caps.add.iter().any(|cap| cap == required))
        }) {
            return Err(ProviderError::Invalid(
                "the bridge template uses --run-as but lacks SETUID and SETGID; create a new template with by substrate setup".into(),
            ));
        }
    }
    let probe = container
        .wakeup_probe
        .as_ref()
        .and_then(|probe| probe.http_get.as_ref())
        .ok_or_else(invalid)?;
    if probe.port != i32::from(BRIDGE_PORT) || probe.path != "/healthz" {
        return Err(invalid());
    }
    let projected = container
        .volume_mounts
        .iter()
        .filter(|mount| mount.mount_path == IDENTITY_DIR)
        .any(|mount| {
            template
                .volumes
                .iter()
                .filter(|volume| volume.name == mount.name)
                .filter_map(|volume| volume.system_info.as_ref())
                .flat_map(|source| &source.data_sources)
                .filter_map(|source| source.actor_metadata.as_ref())
                .any(|source| {
                    [
                        (pb::ActorMetadataField::Atespace, "atespace"),
                        (pb::ActorMetadataField::Name, "name"),
                        (pb::ActorMetadataField::Uid, "uid"),
                    ]
                    .iter()
                    .all(|(field, path)| {
                        source
                            .items
                            .iter()
                            .any(|item| item.field == *field as i32 && item.path == *path)
                    })
                })
        });
    if !projected {
        return Err(invalid());
    }
    let public_key = container
        .env
        .iter()
        .find(|e| e.name == branchyard_bridge::KEY_ENV)
        .map(|e| e.value.clone())
        .ok_or_else(invalid)?;
    branchyard_bridge::Verifier::from_hex(&public_key)?;
    let metadata = template.metadata.as_ref().ok_or_else(invalid)?;
    Ok(TemplateInspection {
        atespace: metadata.atespace.clone(),
        name: metadata.name.clone(),
        image: container.image.clone(),
        bridge_port: BRIDGE_PORT,
        exec: true,
        public_key,
    })
}
