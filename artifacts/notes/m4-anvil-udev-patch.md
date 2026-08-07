# anvil udev.rs LeandrOS patches (M4) — apply to smithay anvil/src/udev.rs

## B2 (is_software rejection) around line ~810:
                        data.frame_finish(node, crtc, metadata);
                    }
                    DrmEvent::Error(error) => {
                        error!("{:?}", error);
                    }
                },
            )
            .unwrap();

        let mut try_initialize_gpu = || {
            let display = unsafe { EGLDisplay::new(gbm.clone()).map_err(DeviceAddError::AddNode)? };
            let egl_device = EGLDevice::device_for_display(&display).map_err(DeviceAddError::AddNode)?;

            // LeandrOS patch: allow software EGL devices (softpipe). LeandrOS has no
            // hardware GL; the entire render+scanout path is softpipe over GBM dumb
            // buffers (the same path kmscube uses). Upstream refuses software devices
            // here, which would leave device_added registering no renderer and make
            // run_udev panic at single_renderer(primary_gpu).
            if egl_device.is_software() {
                tracing::warn!("EGL device is software (softpipe); proceeding anyway (no hw GL on LeandrOS)");
            }

            let render_node = egl_device.try_get_render_node().ok().flatten().unwrap_or(node);

## B1 (ANVIL_DRM_DEVICE direct fallback) around line ~386:
    if let Some((device_id, path)) = primary_device {
        let node = DrmNode::from_dev_id(device_id).expect("failed to get primary node");
        state
            .device_added(node, path)
            .expect("failed to initialize primary node");
    } else if let Ok(var) = std::env::var("ANVIL_DRM_DEVICE") {
        // LeandrOS patch: the libudev shim's DRM-subsystem enumeration returns no
        // devices to the udev Rust crate (input enumeration works, DRM does not),
        // so udev_backend.device_list() is empty and the primary node is never
        // added. When ANVIL_DRM_DEVICE names the node explicitly, add it directly.
        let path = std::path::PathBuf::from(var);
        state
            .device_added(primary_gpu, &path)
            .expect("failed to initialize primary node (ANVIL_DRM_DEVICE direct)");
    }

