use super::*;

#[derive(Clone, Debug, PartialEq)]
pub struct EiRegion {
    pub mapping_id: String,
    pub logical_origin: (i32, i32),
    pub logical_size: (i32, i32),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameGeometry {
    pub pixel_size: (u32, u32),
    pub pipewire_serial: u64,
    /// Cropped or rotated frames need a separate verified transform.
    pub untransformed: bool,
}

/// Stream-to-region calibration only. It proves neither target-window identity
/// nor focus and cannot authorize an input operation on its own.
pub struct StreamMapping {
    generation: u64,
    stream: AuthorizedStream,
    region: EiRegion,
    frame: FrameGeometry,
}
impl StreamMapping {
    pub fn bind(
        generation: u64,
        stream: &AuthorizedStream,
        regions: &[EiRegion],
        frame: FrameGeometry,
    ) -> Result<Self> {
        let id = stream
            .mapping_id
            .as_ref()
            .ok_or(PortalError::MappingUnproven)?;
        let matches = regions
            .iter()
            .filter(|r| &r.mapping_id == id)
            .collect::<Vec<_>>();
        let [region] = matches.as_slice() else {
            return Err(PortalError::MappingUnproven);
        };
        if generation == 0
            || stream.source_type != 1
            || !frame.untransformed
            || frame.pixel_size.0 == 0
            || frame.pixel_size.1 == 0
            || stream.pipewire_serial != Some(frame.pipewire_serial)
            || frame.pipewire_serial == 0
            || stream.logical_origin != Some(region.logical_origin)
            || stream.logical_size != Some(region.logical_size)
            || region.logical_size.0 <= 0
            || region.logical_size.1 <= 0
        {
            return Err(PortalError::MappingUnproven);
        }
        Ok(Self {
            generation,
            stream: stream.clone(),
            region: (*region).clone(),
            frame,
        })
    }
    pub fn map_fraction(
        &self,
        generation: u64,
        stream: &AuthorizedStream,
        frame: FrameGeometry,
        fraction: (f64, f64),
    ) -> Result<(f64, f64)> {
        if generation != self.generation
            || stream != &self.stream
            || frame != self.frame
            || !fraction.0.is_finite()
            || !fraction.1.is_finite()
            || !(0.0..1.0).contains(&fraction.0)
            || !(0.0..1.0).contains(&fraction.1)
        {
            return Err(PortalError::MappingUnproven);
        }
        Ok((
            f64::from(self.region.logical_origin.0)
                + fraction.0 * f64::from(self.region.logical_size.0),
            f64::from(self.region.logical_origin.1)
                + fraction.1 * f64::from(self.region.logical_size.1),
        ))
    }
}
