//! Host extensions: named host calls that a custom engine build adds for its
//! plugins (`host-ext.ext-call`).
//!
//! An extension registers under a namespace; a plugin calls
//! `<namespace>.<method>` and must declare the capability `ext:<namespace>`
//! in its manifest. The extension runs in the host, so secrets it uses never
//! enter the plugin's sandbox. The stock engine registers none, and a call
//! then fails with "no extension".

use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, RwLock},
};

/// The future an extension call returns: JSON text, or an error message.
pub type HostExtFuture = Pin<Box<dyn Future<Output = Result<String, String>> + Send>>;

/// One host extension.
pub trait HostExtension: Send + Sync {
    /// Handle `method` for the plugin named `plugin`, with JSON `args_json`.
    fn call(&self, plugin: &str, method: &str, args_json: &str) -> HostExtFuture;
}

static REGISTRY: RwLock<Vec<(String, Arc<dyn HostExtension>)>> = RwLock::new(Vec::new());

/// Register `extension` under `namespace`, replacing an earlier one.
pub fn register(namespace: &str, extension: Arc<dyn HostExtension>) {
    let mut registry = REGISTRY
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.retain(|(name, _)| name != namespace);
    registry.push((namespace.to_owned(), extension));
}

/// The extension registered under `namespace`.
pub fn lookup(namespace: &str) -> Option<Arc<dyn HostExtension>> {
    let registry = REGISTRY
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry
        .iter()
        .find(|(name, _)| name == namespace)
        .map(|(_, ext)| Arc::clone(ext))
}

/// The manifest capability a plugin needs to call `namespace`.
pub fn capability(namespace: &str) -> String {
    format!("ext:{namespace}")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    impl HostExtension for Echo {
        fn call(&self, plugin: &str, method: &str, args_json: &str) -> HostExtFuture {
            let out = format!("{plugin}:{method}:{args_json}");
            Box::pin(async move { Ok(out) })
        }
    }

    #[tokio::test]
    async fn host_ext_registered_extension_is_found_by_namespace() {
        register("test-echo", Arc::new(Echo));
        assert!(lookup("test-missing").is_none());
        let ext = lookup("test-echo").expect("registered");
        assert_eq!(ext.call("p", "m", "{}").await, Ok("p:m:{}".to_owned()));
        assert_eq!(capability("test-echo"), "ext:test-echo");
    }
}
