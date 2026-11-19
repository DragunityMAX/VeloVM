use crate::models::HostConfig;
use crate::models::VmConfig;
use crate::models::default_runtime_dir;
use anyhow::Context;
use anyhow::bail;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct RegistryStore {
    data_dir: PathBuf,
}

impl RegistryStore {
    pub fn new(data_dir: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let data_dir = data_dir.into();
        fs::create_dir_all(data_dir.join("vms"))
            .with_context(|| format!("creating VM store dir under {}", data_dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700));
            let _ = fs::set_permissions(data_dir.join("vms"), fs::Permissions::from_mode(0o700));
        }
        Ok(Self { data_dir })
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn host_config_path(&self) -> PathBuf {
        self.data_dir.join("config.toml")
    }

    pub fn token_path(&self) -> PathBuf {
        self.data_dir.join("daemon.token")
    }

    pub fn vms_dir(&self) -> PathBuf {
        self.data_dir.join("vms")
    }

    pub fn vm_dir(&self, id: Uuid) -> PathBuf {
        self.vms_dir().join(id.to_string())
    }

    pub fn vm_config_path(&self, id: Uuid) -> PathBuf {
        self.vm_dir(id).join("vm.toml")
    }

    pub fn vm_log_path(&self, id: Uuid) -> PathBuf {
        self.vm_dir(id).join("logs").join("openvmm.log")
    }

    pub fn load_or_init_host_config(&self) -> anyhow::Result<HostConfig> {
        let path = self.host_config_path();
        if path.exists() {
            let text = fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let mut cfg: HostConfig = toml::from_str(&text)
                .with_context(|| format!("parsing {}", path.display()))?;
            if cfg.data_dir.as_os_str().is_empty() {
                cfg.data_dir = self.data_dir.clone();
            }
            if cfg.runtime_dir.as_os_str().is_empty() {
                cfg.runtime_dir = default_runtime_dir(&self.data_dir);
            }
            Ok(cfg)
        } else {
            let cfg = HostConfig {
                data_dir: self.data_dir.clone(),
                runtime_dir: default_runtime_dir(&self.data_dir),
                ..Default::default()
            };
            self.save_host_config(&cfg)?;
            Ok(cfg)
        }
    }

    pub fn save_host_config(&self, cfg: &HostConfig) -> anyhow::Result<()> {
        let text = toml::to_string_pretty(cfg).context("serializing host config")?;
        atomic_write(&self.host_config_path(), text.as_bytes(), false)
    }

    pub fn load_or_create_token(&self) -> anyhow::Result<String> {
        let path = self.token_path();
        if path.exists() {
            let token = fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?
                .trim()
                .to_string();
            if !token.is_empty() {
                return Ok(token);
            }
        }
        let a = Uuid::new_v4().as_u128();
        let b = Uuid::new_v4().as_u128();
        let token = format!("{a:032x}{b:032x}");
        atomic_write(&path, token.as_bytes(), true)?;
        Ok(token)
    }

    pub fn read_token(&self) -> anyhow::Result<String> {
        let path = self.token_path();
        let token = fs::read_to_string(&path)
            .with_context(|| format!("reading daemon token at {}", path.display()))?
            .trim()
            .to_string();
        if token.is_empty() {
            bail!("daemon token file {} is empty", path.display());
        }
        Ok(token)
    }

    pub fn load_all_vms(&self) -> anyhow::Result<Vec<VmConfig>> {
        let vms_dir = self.vms_dir();
        if !vms_dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&vms_dir)
            .with_context(|| format!("reading {}", vms_dir.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let cfg_path = entry.path().join("vm.toml");
            if !cfg_path.is_file() {
                continue;
            }
            let text = fs::read_to_string(&cfg_path)
                .with_context(|| format!("reading {}", cfg_path.display()))?;
            let vm: VmConfig = toml::from_str(&text)
                .with_context(|| format!("parsing {}", cfg_path.display()))?;
            out.push(vm);
        }
        out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    pub fn save_vm(&self, vm: &VmConfig) -> anyhow::Result<()> {
        vm.validate()?;
        let vm_dir = self.vm_dir(vm.id);
        fs::create_dir_all(vm_dir.join("logs"))
            .with_context(|| format!("creating VM directory {}", vm_dir.display()))?;
        let text = toml::to_string_pretty(vm).context("serializing VM config")?;
        atomic_write(&self.vm_config_path(vm.id), text.as_bytes(), false)
    }

    pub fn delete_vm_dir(&self, id: Uuid) -> anyhow::Result<()> {
        let vm_dir = self.vm_dir(id);
        if vm_dir.exists() {
            fs::remove_dir_all(&vm_dir)
                .with_context(|| format!("deleting VM directory {}", vm_dir.display()))?;
        }
        Ok(())
    }
}

pub fn atomic_write(path: &Path, data: &[u8], owner_only: bool) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .context("target path has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating directory {}", parent.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temp file in {}", parent.display()))?;
    #[cfg(unix)]
    if owner_only {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .context("setting owner-only permissions")?;
    }
    #[cfg(not(unix))]
    let _ = owner_only;
    tmp.write_all(data)
        .with_context(|| format!("writing temp file for {}", path.display()))?;
    tmp.as_file()
        .sync_all()
        .with_context(|| format!("syncing temp file for {}", path.display()))?;
    tmp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("persisting {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::VmPreset;

    #[test]
    fn host_and_vm_persistence_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::new(dir.path()).unwrap();

        let token1 = store.load_or_create_token().unwrap();
        let token2 = store.load_or_create_token().unwrap();
        assert_eq!(token1, token2);
        assert_eq!(token1.len(), 64);

        let host_cfg = store.load_or_init_host_config().unwrap();
        assert_eq!(host_cfg.data_dir, dir.path());

        let vm_dir = store.vm_dir(Uuid::new_v4());
        let vm = VmConfig::from_preset("Win11-Test", VmPreset::Windows11, &vm_dir);
        store.save_vm(&vm).unwrap();

        let loaded = store.load_all_vms().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, vm.id);
        assert_eq!(loaded[0].name, "Win11-Test");

        store.delete_vm_dir(vm.id).unwrap();
        assert!(store.load_all_vms().unwrap().is_empty());
    }
}
