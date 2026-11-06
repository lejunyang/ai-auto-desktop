//! Per-window calibration for Qt's logical AT-SPI coordinates on X11.
use super::*;
use aad_linux::window_geometry::{self, ClientGeometry};

#[derive(Clone)]
pub(super) struct CoordinateMap {
    window: NativeRef,
    raw: Bounds,
    physical: Bounds,
    client: u32,
    process: u32,
    members: BTreeSet<String>,
}

fn bounds(client: &ClientGeometry) -> Bounds {
    Bounds {
        x: client.x,
        y: client.y,
        width: client.width,
        height: client.height,
    }
}

fn calibration(raw: Bounds, physical: Bounds) -> bool {
    if raw.width <= 0 || raw.height <= 0 || physical.width <= 0 || physical.height <= 0 {
        return false;
    }
    let sx = f64::from(physical.width) / f64::from(raw.width);
    let sy = f64::from(physical.height) / f64::from(raw.height);
    (0.25..=8.0).contains(&sx)
        && (0.25..=8.0).contains(&sy)
        && (sx - sy).abs() <= 1.0 / f64::from(raw.width) + 1.0 / f64::from(raw.height)
}

fn rounded(value: f64, up: bool) -> Result<i32, AutomationError> {
    let value = if up { value.ceil() } else { value.floor() };
    if !value.is_finite() || value < f64::from(i32::MIN) || value > f64::from(i32::MAX) {
        return Err(driver_error(
            "DRIVER.COORDINATE_UNPROVEN",
            "coordinate conversion overflowed",
        ));
    }
    Ok(value as i32)
}

impl CoordinateMap {
    fn physical_bounds(&self, value: Bounds) -> Result<Bounds, AutomationError> {
        if value == self.raw {
            return Ok(self.physical);
        }
        let sx = f64::from(self.physical.width) / f64::from(self.raw.width);
        let sy = f64::from(self.physical.height) / f64::from(self.raw.height);
        // Inward rounding keeps action fractions inside the observed element.
        let x = rounded(
            f64::from(self.physical.x) + (f64::from(value.x) - f64::from(self.raw.x)) * sx,
            true,
        )?;
        let y = rounded(
            f64::from(self.physical.y) + (f64::from(value.y) - f64::from(self.raw.y)) * sy,
            true,
        )?;
        let right = rounded(
            f64::from(self.physical.x)
                + (f64::from(value.x) + f64::from(value.width) - f64::from(self.raw.x)) * sx,
            false,
        )?;
        let bottom = rounded(
            f64::from(self.physical.y)
                + (f64::from(value.y) + f64::from(value.height) - f64::from(self.raw.y)) * sy,
            false,
        )?;
        Ok(Bounds {
            x,
            y,
            width: right.checked_sub(x).unwrap_or(0).max(0),
            height: bottom.checked_sub(y).unwrap_or(0).max(0),
        })
    }
    fn logical_point(&self, point: (i32, i32)) -> Result<(i32, i32), AutomationError> {
        Ok((
            rounded(
                f64::from(self.raw.x)
                    + (f64::from(point.0) - f64::from(self.physical.x)) * f64::from(self.raw.width)
                        / f64::from(self.physical.width),
                false,
            )?,
            rounded(
                f64::from(self.raw.y)
                    + (f64::from(point.1) - f64::from(self.physical.y))
                        * f64::from(self.raw.height)
                        / f64::from(self.physical.height),
                false,
            )?,
        ))
    }
}

impl LinuxBackend {
    // Scope narrowing never narrows the uniqueness proof for native windows.
    // Only direct-window identity/geometry metadata is read; these siblings
    // are never added to the caller's observation or action membership.
    fn application_coordinate_windows(
        &self,
        application: &Map<String, Value>,
        deadline: Instant,
    ) -> Result<Vec<(NativeRef, String, Bounds)>, AutomationError> {
        let app = NativeRef {
            bus_name: application
                .get("bus_name")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    driver_error("DRIVER.COORDINATE_UNPROVEN", "application bus missing")
                })?
                .to_string(),
            object_path: application
                .get("object_path")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    driver_error("DRIVER.COORDINATE_UNPROVEN", "application root missing")
                })?
                .to_string(),
        };
        let children = self.child_references(&app, deadline)?;
        let count = zbus::block_on(self.accessible(&app)?.child_count())
            .map_err(|e| native_failure("coordinate window count", &app, e))?;
        if usize::try_from(count).ok() != Some(children.returned_count)
            || children.items.len() > 128
        {
            return Err(driver_error(
                "DRIVER.COORDINATE_UNPROVEN",
                "complete direct-window inventory unavailable",
            ));
        }
        let mut result = Vec::new();
        for native in children.items {
            self.check_cancelled(false)?;
            remaining(deadline, false)?;
            if native.bus_name != app.bus_name {
                return Err(driver_error(
                    "DRIVER.COORDINATE_UNPROVEN",
                    "application window belongs to another bus",
                ));
            }
            let name = zbus::block_on(self.accessible(&native)?.name())
                .map_err(|e| native_failure("coordinate window name", &native, e))?;
            let (x, y, width, height) = zbus::block_on(
                self.component_proxy(&native)?
                    .get_extents(CoordType::Screen),
            )
            .map_err(|e| native_failure("coordinate window geometry", &native, e))?;
            if width > 0 && height > 0 {
                result.push((
                    native,
                    name,
                    Bounds {
                        x,
                        y,
                        width,
                        height,
                    },
                ));
            }
        }
        self.check_cancelled(false)?;
        remaining(deadline, false)?;
        Ok(result)
    }

    fn read_client_geometry(
        &self,
        process: u32,
        deadline: Instant,
    ) -> Result<Vec<ClientGeometry>, AutomationError> {
        if std::env::var("XDG_SESSION_TYPE").as_deref() != Ok("x11")
            || std::env::var_os("DISPLAY").is_none_or(|value| value.is_empty())
        {
            return Err(driver_error(
                "DRIVER.COORDINATE_UNPROVEN",
                "physical coordinate calibration requires an X11 session",
            ));
        }
        self.check_cancelled(false)?;
        remaining(deadline, false)?;
        if self
            .geometry_read_busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(driver_error(
                "DRIVER.RESOURCE_BUSY",
                "a previous X11 geometry read is still running",
            ));
        }
        struct Permit(Arc<AtomicBool>);
        impl Drop for Permit {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let permit = Permit(self.geometry_read_busy.clone());
        let (send, recv) = mpsc::sync_channel(1);
        let until = deadline.min(Instant::now() + Duration::from_secs(2));
        std::thread::Builder::new()
            .name("aad-window-geometry".into())
            .spawn(move || {
                let _permit = permit;
                let _ = send.send(window_geometry::clients(process, until));
            })
            .map_err(|_| {
                driver_error("DRIVER.UNAVAILABLE", "cannot start window geometry reader")
            })?;
        loop {
            self.check_cancelled(false)?;
            remaining(until, false)?;
            match recv.recv_timeout(Duration::from_millis(5)) {
                Ok(result) => {
                    self.check_cancelled(false)?;
                    remaining(until, false)?;
                    return result
                        .map_err(|reason| driver_error("DRIVER.COORDINATE_UNPROVEN", reason));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => {
                    return Err(driver_error(
                        "DRIVER.COORDINATE_UNPROVEN",
                        "window geometry reader stopped",
                    ));
                }
            }
        }
    }

    pub(super) fn normalize_coordinates(
        &self,
        snapshot: &mut BackendSnapshot,
        deadline: Instant,
    ) -> Result<(), AutomationError> {
        if snapshot
            .application
            .get("toolkit_name")
            .and_then(Value::as_str)
            != Some("Qt")
        {
            return Ok(());
        }
        let bus = snapshot
            .application
            .get("bus_name")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                driver_error(
                    "DRIVER.COORDINATE_UNPROVEN",
                    "Qt application has no native bus identity",
                )
            })?;
        {
            let mut known = self.coordinate_qt_buses.lock().map_err(|_| {
                driver_error("DRIVER.INTERNAL", "coordinate identity cache unavailable")
            })?;
            if !known.contains(bus) && known.len() >= 512 {
                return Err(driver_error(
                    "DRIVER.OUTPUT_TOO_LARGE",
                    "Qt coordinate identity limit reached; create a new session",
                ));
            }
            known.insert(bus.to_string());
        }
        let process = snapshot
            .application
            .get("process_id")
            .and_then(Value::as_u64)
            .and_then(|p| u32::try_from(p).ok())
            .unwrap_or(0);
        let mut clients = match self.read_client_geometry(process, deadline) {
            Ok(clients) => clients,
            Err(error) if matches!(error.code.as_str(), "DRIVER.TIMEOUT" | "DRIVER.CANCELLED") => {
                return Err(error);
            }
            Err(error) => {
                snapshot
                    .application
                    .insert("coordinate_diagnostic".into(), json!(error.code));
                Vec::new()
            }
        };
        // The Rust X11 reader does not guess an encoding for COMPOUND_TEXT.
        // The existing read-only helper returns a lossless conversion, bound
        // to the same exact XID and exact property bytes captured above.
        if clients.iter().any(|c| c.compound_title.is_some()) {
            if let Ok(input) = self.probe_input(deadline) {
                for client in &mut clients {
                    let Some(raw) = &client.compound_title else {
                        continue;
                    };
                    self.check_cancelled(false)?;
                    remaining(deadline, false)?;
                    let Some(info) =
                        self.window_info(process, bounds(client), &input.path, deadline)
                    else {
                        continue;
                    };
                    let hex: String = raw.iter().map(|byte| format!("{byte:02x}")).collect();
                    if info["window"].as_u64() == Some(u64::from(client.id))
                        && info["wm_name_raw_hex"].as_str() == Some(hex.as_str())
                    {
                        client.legacy_title = info["wm_name_utf8"]
                            .as_str()
                            .filter(|s| !s.contains('\0'))
                            .map(str::to_owned);
                    }
                }
            }
            self.check_cancelled(false)?;
            remaining(deadline, false)?;
        }
        let windows = match self.application_coordinate_windows(&snapshot.application, deadline) {
            Ok(windows) => windows,
            Err(error) if matches!(error.code.as_str(), "DRIVER.TIMEOUT" | "DRIVER.CANCELLED") => {
                return Err(error);
            }
            Err(error) => {
                remaining(deadline, false)?;
                snapshot
                    .application
                    .insert("coordinate_diagnostic".into(), json!(error.code));
                Vec::new()
            }
        };
        let mut maps = Vec::new();
        for window in snapshot.nodes.iter().filter(|n| n.parent_index == Some(0)) {
            let Some(raw) = window.bounds.filter(|b| b.width > 0 && b.height > 0) else {
                continue;
            };
            let matches = clients
                .iter()
                .filter(|c| {
                    window
                        .name
                        .as_deref()
                        .is_some_and(|name| c.title_matches(name))
                        && calibration(raw, bounds(c))
                })
                .collect::<Vec<_>>();
            let [client] = matches.as_slice() else {
                // Keep semantic observation; synthetic actions must not guess
                // between clients or use toolkit coordinates as physical ones.
                continue;
            };
            let candidates = windows
                .iter()
                .filter(|(_, name, bounds)| {
                    client.title_matches(name) && calibration(*bounds, self::bounds(client))
                })
                .collect::<Vec<_>>();
            if candidates.len() != 1 || candidates[0].0 != window.native || candidates[0].2 != raw {
                continue;
            }
            maps.push(CoordinateMap {
                window: window.native.clone(),
                raw,
                physical: bounds(client),
                client: client.id,
                process,
                members: BTreeSet::new(),
            });
        }
        let mut counts = BTreeMap::new();
        for map in &maps {
            *counts.entry(map.client).or_insert(0usize) += 1;
        }
        maps.retain(|map| counts[&map.client] == 1);
        for index in 0..snapshot.nodes.len() {
            let mut ancestor = index;
            let mut visited = BTreeSet::new();
            while let Some(parent) = snapshot.nodes[ancestor].parent_index {
                if parent == 0 || !visited.insert(ancestor) {
                    break;
                }
                ancestor = parent;
            }
            if snapshot.nodes[ancestor].parent_index != Some(0) {
                continue;
            }
            let native = snapshot.nodes[ancestor].native.clone();
            let node = &mut snapshot.nodes[index];
            if let Some(map) = maps.iter_mut().find(|m| m.window == native) {
                map.members.insert(node.native.object_path.clone());
                if let Some(raw) = node.bounds {
                    node.bounds = Some(map.physical_bounds(raw)?);
                    node.provenance.insert("atspi_bounds".into(), raw.to_json());
                    node.provenance
                        .insert("coordinate_space".into(), json!("screen_physical"));
                    node.provenance
                        .insert("coordinate_window".into(), json!(map.client));
                }
            } else {
                node.actions.retain(|a| {
                    !matches!(
                        a.as_str(),
                        "pointer_click"
                            | "pointer_gesture"
                            | "drag"
                            | "key_chord"
                            | "type_text"
                            | "paste"
                    )
                });
                node.provenance
                    .insert("coordinate_space".into(), json!("unproven_toolkit"));
            }
        }
        let mut cache = self
            .coordinate_maps
            .lock()
            .map_err(|_| driver_error("DRIVER.INTERNAL", "coordinate cache unavailable"))?;
        cache.retain(|map| {
            !snapshot
                .nodes
                .iter()
                .any(|node| node.parent_index == Some(0) && node.native == map.window)
        });
        for map in maps {
            cache.retain(|m| m.window != map.window);
            cache.push_back(map);
        }
        while cache.len() > 32 {
            cache.pop_front();
        }
        Ok(())
    }

    fn map_for(
        &self,
        target: &NativeRef,
        deadline: Instant,
    ) -> Result<Option<CoordinateMap>, AutomationError> {
        let known = self
            .coordinate_qt_buses
            .lock()
            .map_err(|_| driver_error("DRIVER.INTERNAL", "coordinate cache unavailable"))?
            .contains(&target.bus_name);
        if !known {
            return Ok(None);
        }
        let mut current = target.clone();
        let mut seen = BTreeSet::new();
        for _ in 0..super::super::MAX_DEPTH {
            remaining(deadline, false)?;
            if !seen.insert(current.object_path.clone()) || current.bus_name != target.bus_name {
                break;
            }
            if let Some(found) = self.coordinate_maps.lock().ok().and_then(|maps| {
                maps.iter()
                    .rev()
                    .find(|m| {
                        m.window.bus_name == current.bus_name
                            && (m.window == current || m.members.contains(&current.object_path))
                    })
                    .cloned()
            }) {
                return Ok(Some(found));
            }
            let Some(parent) = self.parent(&current, deadline)? else {
                break;
            };
            current = parent;
        }
        Err(driver_error(
            "DRIVER.COORDINATE_UNPROVEN",
            "target has no retained coordinate window",
        ))
    }

    fn validate_coordinate_map(
        &self,
        map: &CoordinateMap,
        deadline: Instant,
    ) -> Result<(), AutomationError> {
        let (x, y, width, height) = zbus::block_on(
            self.component_proxy(&map.window)?
                .get_extents(CoordType::Screen),
        )
        .map_err(|e| native_failure("coordinate window bounds", &map.window, e))?;
        if (Bounds {
            x,
            y,
            width,
            height,
        }) != map.raw
            || self.process_id(&map.window.bus_name) != Some(map.process)
        {
            return Err(super::super::stale(
                "coordinate window identity or geometry changed",
            ));
        }
        let clients = self.read_client_geometry(map.process, deadline)?;
        if !clients
            .iter()
            .any(|c| c.id == map.client && bounds(c) == map.physical)
        {
            return Err(super::super::stale("physical coordinate window changed"));
        }
        self.check_cancelled(false)?;
        remaining(deadline, false)?;
        Ok(())
    }

    pub(super) fn verify_coordinate_target(
        &self,
        native: &NativeRef,
        deadline: Instant,
    ) -> Result<(), AutomationError> {
        if let Some(map) = self.map_for(native, deadline)? {
            self.validate_coordinate_map(&map, deadline)?;
        }
        Ok(())
    }

    pub(super) fn physical_window_bounds(
        &self,
        native: &NativeRef,
        raw: Bounds,
        deadline: Instant,
    ) -> Result<Bounds, AutomationError> {
        if let Some(map) = self.map_for(native, deadline)? {
            if map.window != *native || map.raw != raw {
                return Err(super::super::stale("coordinate window changed"));
            }
            self.validate_coordinate_map(&map, deadline)?;
            Ok(map.physical)
        } else {
            Ok(raw)
        }
    }

    pub(super) fn accessibility_point(
        &self,
        native: &NativeRef,
        point: (i32, i32),
        deadline: Instant,
    ) -> Result<(i32, i32), AutomationError> {
        if let Some(map) = self.map_for(native, deadline)? {
            self.validate_coordinate_map(&map, deadline)?;
            map.logical_point(point)
        } else {
            Ok(point)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn per_window_mapping_handles_fractional_scale_and_negative_origins() {
        let map = CoordinateMap {
            window: NativeRef {
                bus_name: ":1.2".into(),
                object_path: "/w".into(),
            },
            raw: Bounds {
                x: -100,
                y: 20,
                width: 400,
                height: 200,
            },
            physical: Bounds {
                x: -300,
                y: 100,
                width: 600,
                height: 300,
            },
            client: 1,
            process: 1,
            members: BTreeSet::new(),
        };
        assert!(calibration(map.raw, map.physical));
        assert_eq!(
            map.physical_bounds(Bounds {
                x: -90,
                y: 30,
                width: 20,
                height: 20
            })
            .unwrap(),
            Bounds {
                x: -285,
                y: 115,
                width: 30,
                height: 30
            }
        );
        assert_eq!(map.logical_point((-270, 130)).unwrap(), (-80, 40));
        assert!(!calibration(
            map.raw,
            Bounds {
                width: 600,
                height: 600,
                ..map.physical
            }
        ));
        assert!(!calibration(
            Bounds {
                width: 0,
                ..map.raw
            },
            map.physical
        ));
    }
}
