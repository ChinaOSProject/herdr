use std::collections::HashMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::api::schema::InstalledPluginInfo;

const LEASE_FILE: &str = ".in-use";
pub(crate) type Leases = HashMap<PathBuf, Arc<File>>;

pub(crate) fn create_lease(installation: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(installation.join(LEASE_FILE))?;
    file.lock_shared()?;
    Ok(file)
}

fn lease_path(checkout: &Path) -> Option<PathBuf> {
    (checkout.file_name()? == "checkout").then(|| {
        let path = checkout.parent()?;
        Some(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
    })?
}

pub(crate) fn command_lease(leases: &Leases, plugin: &InstalledPluginInfo) -> Option<Arc<File>> {
    let installation = lease_path(Path::new(plugin.source.managed_path.as_deref()?))?;
    leases.get(&installation).cloned()
}

// The caller holds the registry lock, preventing cleanup between registry
// selection and acquisition. Missing markers denote untracked installations.
fn retain_checkout(leases: &mut Leases, checkout: &Path) -> io::Result<()> {
    let Some(installation) = lease_path(checkout) else {
        return Ok(());
    };
    if leases.contains_key(&installation) {
        return Ok(());
    }
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .open(installation.join(LEASE_FILE))
    {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    file.lock_shared()?;
    leases.insert(installation, Arc::new(file));
    Ok(())
}

pub(crate) fn load(leases: &mut Leases) -> io::Result<Vec<InstalledPluginInfo>> {
    crate::persist::plugin_registry::read(|entries| {
        for entry in &entries {
            if let Some(path) = &entry.source.managed_path {
                retain_checkout(leases, Path::new(path))?;
            }
        }
        Ok(entries)
    })
}

fn installations() -> io::Result<Vec<PathBuf>> {
    let root = crate::plugin_paths::managed_plugins_dir().join("github-installations");
    let plugins = match std::fs::read_dir(root) {
        Ok(plugins) => plugins,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut result = Vec::new();
    for plugin in plugins {
        let plugin = plugin?;
        if !plugin.file_type()?.is_dir() {
            continue;
        }
        for generation in std::fs::read_dir(plugin.path())? {
            let generation = generation?;
            if generation.file_type()?.is_dir() {
                result.push(generation.path().canonicalize()?);
            }
        }
    }
    Ok(result)
}

pub(crate) fn retain_startup(leases: &mut Leases, restored_cwds: &[PathBuf]) -> io::Result<()> {
    crate::persist::plugin_registry::read(|_| {
        for installation in installations()? {
            if leases.contains_key(&installation) {
                continue;
            }
            let file = match OpenOptions::new()
                .read(true)
                .write(true)
                .open(installation.join(LEASE_FILE))
            {
                Ok(file) => file,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err),
            };
            let referenced = restored_cwds.iter().any(|cwd| {
                cwd.canonicalize()
                    .unwrap_or_else(|_| cwd.clone())
                    .starts_with(installation.join("checkout"))
            });
            match file.try_lock() {
                Ok(()) if !referenced => continue,
                Ok(()) => file.unlock()?,
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(err)) => return Err(err),
            }
            // Also retain generations held by the outgoing server during handoff.
            file.lock_shared()?;
            leases.insert(installation, Arc::new(file));
        }
        Ok(())
    })
}

pub(crate) fn cleanup() -> io::Result<()> {
    crate::persist::plugin_registry::read(|entries| {
        for installation in installations()? {
            let checkout = installation.join("checkout");
            if entries.iter().any(|entry| {
                entry.source.managed_path.as_deref().is_some_and(|path| {
                    let path = Path::new(path);
                    path == checkout
                        || path.canonicalize().ok().as_deref() == Some(checkout.as_path())
                })
            }) {
                continue;
            }
            let Some(component) = installation.parent().and_then(Path::file_name) else {
                continue;
            };
            let mutation_path = crate::plugin_paths::managed_plugins_dir()
                .join(".locks")
                .join(format!(".{}.lock", component.to_string_lossy()));
            let mutation = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(mutation_path)?;
            match mutation.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => continue,
                Err(TryLockError::Error(err)) => return Err(err),
            }
            let lease = match OpenOptions::new()
                .read(true)
                .write(true)
                .open(installation.join(LEASE_FILE))
            {
                Ok(file) => file,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err),
            };
            match lease.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => continue,
                Err(TryLockError::Error(err)) => return Err(err),
            }
            // Windows cannot remove the open lease file. Registry + mutation
            // locks exclude new readers/installers while the handle is closed.
            drop(lease);
            std::fs::remove_dir_all(&installation)?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_config(test: impl FnOnce()) {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "herdr-plugin-cleanup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", &root);
        std::fs::create_dir_all(crate::plugin_paths::managed_plugins_dir().join(".locks")).unwrap();
        test();
        match previous {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    fn installation(id: &str) -> (PathBuf, InstalledPluginInfo) {
        let root = crate::plugin_paths::create_managed_installation(id).unwrap();
        let checkout = root.join("checkout");
        std::fs::create_dir(&checkout).unwrap();
        std::fs::write(
            checkout.join("herdr-plugin.toml"),
            format!(
                "id = {id:?}\nname = 'Cleanup'\nversion = '0.1.0'\nmin_herdr_version = '0.6.10'\n"
            ),
        )
        .unwrap();
        let mut plugin =
            crate::app::load_plugin_manifest(&checkout.to_string_lossy(), true).unwrap();
        plugin.source.managed_path = Some(checkout.display().to_string());
        drop(create_lease(&root).unwrap());
        (root, plugin)
    }

    #[test]
    fn cleanup_waits_for_all_servers_and_command_references_and_preserves_current_paths() {
        with_config(|| {
            let (current, mut registered) = installation("example.current");
            // Registry paths and config paths can use different aliases.
            registered.source.managed_path =
                Some(current.join("checkout/../checkout").display().to_string());
            let (retired, old) = installation("example.retired");
            let untracked =
                crate::plugin_paths::create_managed_installation("example.legacy").unwrap();
            crate::persist::plugin_registry::update(|entries| {
                *entries = vec![registered.clone(), old.clone()];
            })
            .unwrap();
            let mut first_server = Leases::new();
            let mut second_server = Leases::new();
            load(&mut first_server).unwrap();
            load(&mut second_server).unwrap();
            let command = command_lease(&first_server, &old).unwrap();
            crate::persist::plugin_registry::update(|entries| *entries = vec![registered]).unwrap();
            drop(first_server);
            cleanup().unwrap();
            assert!(retired.exists());
            drop(second_server);
            cleanup().unwrap();
            assert!(retired.exists(), "running commands outlive the server map");
            drop(command);
            cleanup().unwrap();
            assert!(!retired.exists());
            assert!(
                current.exists(),
                "registered alias must protect the current files"
            );
            assert!(
                untracked.exists(),
                "untracked installations are never reclaimed"
            );

            let (building, _) = installation("example.building");
            let mutation = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(crate::plugin_paths::managed_checkout_lock_path(
                    "example.building",
                ))
                .unwrap();
            mutation.lock().unwrap();
            cleanup().unwrap();
            assert!(
                building.exists(),
                "a building checkout is not yet registered"
            );
            drop(mutation);
            cleanup().unwrap();
            assert!(!building.exists());
        });
    }

    #[test]
    fn startup_retains_restored_and_handoff_generations_and_rejects_pin_errors() {
        with_config(|| {
            let (restored, _) = installation("example.restored");
            let (handoff, _) = installation("example.handoff");
            let outgoing = OpenOptions::new()
                .read(true)
                .write(true)
                .open(handoff.join(LEASE_FILE))
                .unwrap();
            outgoing.lock_shared().unwrap();
            let mut incoming = Leases::new();
            retain_startup(&mut incoming, &[restored.join("checkout")]).unwrap();
            drop(outgoing);
            cleanup().unwrap();
            assert!(restored.exists());
            assert!(handoff.exists());
            drop(incoming);
            cleanup().unwrap();
            assert!(!restored.exists());
            assert!(!handoff.exists());

            let broken =
                crate::plugin_paths::create_managed_installation("example.broken").unwrap();
            std::fs::create_dir(broken.join(LEASE_FILE)).unwrap();
            let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let app = crate::app::App::try_new(
                &crate::config::Config::default(),
                crate::app::AppPolicy {
                    persist_plugin_registry: true,
                    ..crate::app::AppPolicy::TEST
                },
                None,
                rx,
                crate::api::EventHub::default(),
            );
            assert!(
                app.is_err(),
                "startup cannot restore consumers without their pins"
            );
        });
    }
}
