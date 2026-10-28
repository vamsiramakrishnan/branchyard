use std::path::PathBuf;
use std::time::Duration;

use tonic::metadata::MetadataValue;
use tonic::service::interceptor::InterceptedService;
use tonic::service::Interceptor;
use tonic::transport::Channel;
use tonic::{Request, Status};

use crate::pb::control_client::ControlClient;

pub(crate) type Client = ControlClient<InterceptedService<Channel, Credentials>>;

#[derive(Clone, Debug)]
pub(crate) struct Credentials {
    token_file: Option<PathBuf>,
}

impl Interceptor for Credentials {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        if let Some(path) = &self.token_file {
            let text = std::fs::read_to_string(path).map_err(|_| {
                Status::unauthenticated("could not read the Substrate API token file")
            })?;
            let token = text.trim();
            if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(Status::unauthenticated(
                    "the Substrate API token file must contain one nonempty token",
                ));
            }
            let mut value = MetadataValue::try_from(format!("Bearer {token}"))
                .map_err(|_| Status::unauthenticated("invalid Substrate API token metadata"))?;
            value.set_sensitive(true);
            request.metadata_mut().insert("authorization", value);
        }
        if !request.metadata().contains_key("grpc-timeout") {
            request.set_timeout(Duration::from_secs(300));
        }
        Ok(request)
    }
}

pub(crate) fn client(channel: Channel, token_file: Option<PathBuf>) -> Client {
    ControlClient::with_interceptor(channel, Credentials { token_file })
}
