use log::{debug, error};

pub fn log_err(context: &str, err: &(dyn std::error::Error + Send + Sync)) {
    error!("{context}: {}", error_chain(err));
    debug!("{context}: {err:#?}");
}

fn error_chain(err: &(dyn std::error::Error + Send + Sync)) -> String {
    let mut message = err.to_string();
    let mut source = err.source();

    while let Some(error) = source {
        message.push_str(": ");
        message.push_str(&error.to_string());
        source = error.source();
    }

    message
}
