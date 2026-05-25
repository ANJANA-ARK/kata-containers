// Copyright (c) 2021 Alibaba Cloud
// Copyright (c) 2021, 2023 IBM Corporation
// Copyright (c) 2022 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0
//

use safe_path::scoped_join;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use kata_sys_util::validate::verify_id;
use oci_spec::runtime as oci;

use crate::rpc::CONTAINER_BASE;

use kata_types::mount::KATA_VIRTUAL_VOLUME_IMAGE_GUEST_PULL;
use protocols::agent::Storage;

pub const KATA_IMAGE_WORK_DIR: &str = "/run/kata-containers/image/";
const CONFIG_JSON: &str = "config.json";
const KATA_PAUSE_BUNDLE: &str = "/pause_bundle";

const K8S_CONTAINER_TYPE_KEYS: [&str; 2] = [
    "io.kubernetes.cri.container-type",
    "io.kubernetes.cri-o.ContainerType",
];

// Convenience function to obtain the scope logger.
fn sl() -> slog::Logger {
    slog_scope::logger().new(o!("subsystem" => "image"))
}

// Function to copy a file if it does not exist at the destination
// This function creates a dir, writes a file and if necessary,
// overwrites an existing file.
fn copy_if_not_exists(src: &Path, dst: &Path) -> Result<()> {
    if let Some(dst_dir) = dst.parent() {
        fs::create_dir_all(dst_dir)?;
    }
    fs::copy(src, dst)?;
    Ok(())
}

/// get guest pause image process specification
fn get_pause_image_process() -> Result<oci::Process> {
    let guest_pause_bundle = Path::new(KATA_PAUSE_BUNDLE);
    if !guest_pause_bundle.exists() {
        bail!("Pause image not present in rootfs");
    }
    let guest_pause_config = scoped_join(guest_pause_bundle, CONFIG_JSON)?;

    let image_oci = oci::Spec::load(guest_pause_config.to_str().ok_or_else(|| {
        anyhow!(
            "Failed to load the guest pause image config from {:?}",
            guest_pause_config
        )
    })?)
    .context("load image config file")?;

    let image_oci_process = image_oci.process().as_ref().ok_or_else(|| {
            anyhow!("The guest pause image config does not contain a process specification. Please check the pause image.")
        })?;
    Ok(image_oci_process.clone())
}

/// pause image is packaged in rootfs
pub fn unpack_pause_image(cid: &str) -> Result<String> {
    verify_id(cid).context("The guest pause image cid contains invalid characters.")?;

    let guest_pause_bundle = Path::new(KATA_PAUSE_BUNDLE);
    if !guest_pause_bundle.exists() {
        bail!("Pause image not present in rootfs");
    }
    let guest_pause_config = scoped_join(guest_pause_bundle, CONFIG_JSON)?;
    info!(sl(), "use guest pause image cid {:?}", cid);

    let image_oci = oci::Spec::load(guest_pause_config.to_str().ok_or_else(|| {
        anyhow!(
            "Failed to load the guest pause image config from {:?}",
            guest_pause_config
        )
    })?)
    .context("load image config file")?;

    let image_oci_process = image_oci.process().as_ref().ok_or_else(|| {
            anyhow!("The guest pause image config does not contain a process specification. Please check the pause image.")
        })?;
    info!(
        sl(),
        "pause image oci process {:?}",
        image_oci_process.clone()
    );

    // Ensure that the args vector is not empty before accessing its elements.
    // Check the number of arguments.
    let args = if let Some(args_vec) = image_oci_process.args() {
        args_vec
    } else {
        bail!("The number of args should be greater than or equal to one! Please check the pause image.");
    };

    let pause_bundle = scoped_join(CONTAINER_BASE, cid)?;
    fs::create_dir_all(&pause_bundle)?;
    let pause_rootfs = scoped_join(&pause_bundle, "rootfs")?;
    fs::create_dir_all(&pause_rootfs)?;
    info!(sl(), "pause_rootfs {:?}", pause_rootfs);

    copy_if_not_exists(&guest_pause_config, &pause_bundle.join(CONFIG_JSON))?;
    let arg_path = Path::new(&args[0]).strip_prefix("/")?;
    copy_if_not_exists(
        &guest_pause_bundle.join("rootfs").join(arg_path),
        &pause_rootfs.join(arg_path),
    )?;
    Ok(pause_rootfs.display().to_string())
}

/// check whether the image is for sandbox or for container.
pub fn is_sandbox(image_metadata: &HashMap<String, String>) -> bool {
    let mut is_sandbox = false;
    for key in K8S_CONTAINER_TYPE_KEYS.iter() {
        if let Some(value) = image_metadata.get(key as &str) {
            if value == "sandbox" {
                is_sandbox = true;
                break;
            }
        }
    }
    is_sandbox
}

/// get_process overrides the OCI process spec with pause image process spec if needed
pub fn get_process(
    ocip: &oci::Process,
    oci: &oci::Spec,
    storages: Vec<Storage>,
) -> Result<oci::Process> {
    let mut guest_pull = false;
    for storage in storages {
        if storage.driver == KATA_VIRTUAL_VOLUME_IMAGE_GUEST_PULL {
            guest_pull = true;
            break;
        }
    }
    if guest_pull {
        if let Some(a) = oci.annotations() {
            if is_sandbox(a) {
                return get_pause_image_process();
            }
        }
    }

    Ok(ocip.clone())
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use tempfile::tempdir;
    use oci_spec::runtime as oci;
    use rstest::rstest;

    // Helper to create metadata with annotation
    fn create_metadata(key: &str, value: &str) -> HashMap<String, String> {
        let mut metadata = HashMap::new();
        metadata.insert(key.to_string(), value.to_string());
        metadata
    }

    #[test]
    fn test_copy_if_not_exists_success() {
        let temp_dir = tempdir().expect("Failed to create temp dir");
        let src_path = temp_dir.path().join("source.txt");
        let dst_path = temp_dir.path().join("subdir/dest.txt");

        fs::write(&src_path, b"test content").expect("Failed to write source file");

        let result = copy_if_not_exists(&src_path, &dst_path);
        assert!(result.is_ok(), "copy_if_not_exists should succeed");
        assert!(dst_path.exists(), "Destination file should exist");

        let content = fs::read_to_string(&dst_path).expect("Failed to read dest file");
        assert_eq!(content, "test content");
    }

    #[test]
    fn test_copy_if_not_exists_creates_parent_dirs() {
        let temp_dir = tempdir().expect("Failed to create temp dir");
        let src_path = temp_dir.path().join("source.txt");
        let dst_path = temp_dir.path().join("deep/nested/path/dest.txt");

        fs::write(&src_path, b"test").expect("Failed to write source file");

        let result = copy_if_not_exists(&src_path, &dst_path);
        assert!(result.is_ok(), "Should create parent directories");
        assert!(dst_path.parent().unwrap().exists(), "Parent dirs should exist");
    }

    #[test]
    fn test_copy_if_not_exists_overwrites_existing() {
        let temp_dir = tempdir().expect("Failed to create temp dir");
        let src_path = temp_dir.path().join("source.txt");
        let dst_path = temp_dir.path().join("dest.txt");

        fs::write(&src_path, b"new content").expect("Failed to write source");
        fs::write(&dst_path, b"old content").expect("Failed to write dest");

        let result = copy_if_not_exists(&src_path, &dst_path);
        assert!(result.is_ok(), "Should overwrite existing file");

        let content = fs::read_to_string(&dst_path).expect("Failed to read dest");
        assert_eq!(content, "new content", "Content should be updated");
    }

    #[test]
    fn test_copy_if_not_exists_nonexistent_source() {
        let temp_dir = tempdir().expect("Failed to create temp dir");
        let result = copy_if_not_exists(
            &temp_dir.path().join("nonexistent.txt"),
            &temp_dir.path().join("dest.txt")
        );
        assert!(result.is_err(), "Should fail with nonexistent source");
    }

    #[rstest]
    #[case::cri_sandbox("io.kubernetes.cri.container-type", "sandbox", true)]
    #[case::crio_sandbox("io.kubernetes.cri-o.ContainerType", "sandbox", true)]
    #[case::cri_container("io.kubernetes.cri.container-type", "container", false)]
    #[case::case_sensitive_mismatch("io.kubernetes.cri.container-type", "Sandbox", false)]
    #[case::whitespace_mismatch("io.kubernetes.cri.container-type", " sandbox ", false)]
    fn test_is_sandbox_variations(
        #[case] key: &str,
        #[case] value: &str,
        #[case] expected: bool,
    ) {
        let metadata = create_metadata(key, value);
        assert_eq!(is_sandbox(&metadata), expected);
    }

    #[test]
    fn test_is_sandbox_with_no_metadata() {
        assert!(!is_sandbox(&HashMap::new()), "Empty metadata should not be sandbox");
    }

    #[test]
    fn test_is_sandbox_with_multiple_keys() {
        let mut metadata = create_metadata("io.kubernetes.cri.container-type", "container");
        metadata.insert("io.kubernetes.cri-o.ContainerType".to_string(), "sandbox".to_string());
        assert!(is_sandbox(&metadata), "Should identify sandbox when any key matches");
    }

    #[test]
    fn test_is_sandbox_case_sensitive() {
        let metadata = create_metadata("io.kubernetes.cri.container-type", "Sandbox");
        assert!(!is_sandbox(&metadata), "Should be case-sensitive");
    }

    #[test]
    fn test_is_sandbox_with_extra_whitespace() {
        let metadata = create_metadata("io.kubernetes.cri.container-type", " sandbox ");
        assert!(!is_sandbox(&metadata), "Should not trim whitespace");
    }

    #[test]
    fn test_get_process_without_guest_pull() {
        let process = oci::ProcessBuilder::default()
            .args(vec!["test".to_string()])
            .build()
            .expect("Failed to build process");

        let spec = oci::SpecBuilder::default()
            .process(process.clone())
            .build()
            .expect("Failed to build spec");

        let storages = vec![];
        let result = get_process(&process, &spec, storages);

        assert!(result.is_ok(), "Should succeed without guest pull");
        let returned_process = result.unwrap();
        assert_eq!(returned_process.args(), process.args());
    }

    #[test]
    fn test_get_process_with_non_guest_pull_storage() {
        let process = oci::ProcessBuilder::default()
            .args(vec!["test".to_string()])
            .build()
            .expect("Failed to build process");

        let spec = oci::SpecBuilder::default()
            .process(process.clone())
            .build()
            .expect("Failed to build spec");

        let mut storage = protocols::agent::Storage::new();
        storage.driver = "other-driver".to_string();
        let storages = vec![storage];

        let result = get_process(&process, &spec, storages);
        assert!(result.is_ok(), "Should succeed with non-guest-pull storage");
        let returned_process = result.unwrap();
        assert_eq!(returned_process.args(), process.args());
    }

    #[test]
    fn test_get_process_with_guest_pull_non_sandbox() {
        let process = oci::ProcessBuilder::default()
            .args(vec!["test".to_string()])
            .build()
            .expect("Failed to build process");

        let mut annotations = HashMap::new();
        annotations.insert("io.kubernetes.cri.container-type".to_string(), "container".to_string());

        let spec = oci::SpecBuilder::default()
            .process(process.clone())
            .annotations(annotations)
            .build()
            .expect("Failed to build spec");

        let mut storage = protocols::agent::Storage::new();
        storage.driver = kata_types::mount::KATA_VIRTUAL_VOLUME_IMAGE_GUEST_PULL.to_string();
        let storages = vec![storage];

        let result = get_process(&process, &spec, storages);
        assert!(result.is_ok(), "Should succeed for non-sandbox with guest pull");
        let returned_process = result.unwrap();
        assert_eq!(returned_process.args(), process.args());
    }

    #[rstest]
    #[case::path_traversal("../malicious")]
    #[case::contains_slashes("container/with/slash")]
    #[case::null_byte("container\0null")]
    #[case::empty_string("")]
    fn test_unpack_pause_image_rejects_invalid_cid(#[case] invalid_cid: &str) {
        let result = unpack_pause_image(invalid_cid);
        assert!(
            result.is_err(),
            "Should reject invalid container ID: '{}'",
            invalid_cid
        );
    }

    #[test]
    fn test_unpack_pause_image_no_pause_bundle() {
        // This test assumes KATA_PAUSE_BUNDLE doesn't exist
        let result = unpack_pause_image("valid-container-id");
        assert!(result.is_err(), "Should fail when pause bundle doesn't exist");
        
        if let Err(e) = result {
            let error_msg = format!("{}", e);
            assert!(error_msg.contains("Pause image not present"), 
                "Error should mention missing pause image: {}", error_msg);
        }
    }

    #[test]
    fn test_get_pause_image_process_no_bundle() {
        let result = get_pause_image_process();
        assert!(result.is_err(), "Should fail when pause bundle doesn't exist");
        
        if let Err(e) = result {
            let error_msg = format!("{}", e);
            assert!(error_msg.contains("Pause image not present"), 
                "Error should mention missing pause image: {}", error_msg);
        }
    }

    #[test]
    fn test_k8s_container_type_keys() {
        assert_eq!(K8S_CONTAINER_TYPE_KEYS.len(), 2);
        assert_eq!(K8S_CONTAINER_TYPE_KEYS[0], "io.kubernetes.cri.container-type");
        assert_eq!(K8S_CONTAINER_TYPE_KEYS[1], "io.kubernetes.cri-o.ContainerType");
    }
}
