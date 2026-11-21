//! #279 P3a: the setting `video_decode`, a child module of `config` (its
//! own file for the 1000-line cap), re-exported there: every caller names
//! it `sp_core::config::{SETTING_VIDEO_DECODE, VideoDecode, video_decode}`.

/// #279 P3a: where playback keeps the pictures the GPU decodes
/// ([`video_decode`]): `gpu` = each picture Media Foundation decodes on the
/// GPU stays in VRAM, in its pipeline's picture ring (no readback, no
/// upload); `software` (the default) = today's path, `video_hw_decode`
/// choosing the decoder. The paced decode producer reads it when it opens
/// a song, so a change applies from the next song; when `gpu` stops taking
/// effect a song playing GPU-resident reads its pictures back from its
/// next one.
pub const SETTING_VIDEO_DECODE: &str = "video_decode";

/// #279 P3a: the two values of [`SETTING_VIDEO_DECODE`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VideoDecode {
    /// Today's path: every picture reaches the program in CPU memory.
    #[default]
    Software,
    /// GPU-resident: a hardware-decoded picture stays in VRAM. It takes
    /// effect only while NDI `SP-program` comes from the GPU
    /// ([`NdiSource::Gpu`](super::NdiSource::Gpu)): the CPU canvas shows a
    /// GPU picture as black.
    Gpu,
}

impl VideoDecode {
    /// The setting's and the telemetry's word: `software` or `gpu`.
    pub fn as_str(self) -> &'static str {
        match self {
            VideoDecode::Software => "software",
            VideoDecode::Gpu => "gpu",
        }
    }
}

/// #279 P3a: a stored `video_decode`: exactly `gpu` (trimmed) keeps the
/// pictures on the GPU; anything else, absent included, is `software`, so a
/// deploy changes nothing on air.
pub fn video_decode(raw: Option<&str>) -> VideoDecode {
    if raw.is_some_and(|v| v.trim() == "gpu") {
        VideoDecode::Gpu
    } else {
        VideoDecode::Software
    }
}

#[cfg(test)]
mod tests {
    use super::{SETTING_VIDEO_DECODE, VideoDecode, video_decode};

    /// #279 P3a: the pictures stay on the GPU only for an explicit `gpu`;
    /// the default (and anything else) is today's software path.
    #[test]
    fn video_decode_is_software_unless_the_setting_says_gpu() {
        assert_eq!(SETTING_VIDEO_DECODE, "video_decode");
        assert_eq!(video_decode(None), VideoDecode::Software, "no setting");
        assert_eq!(video_decode(Some("gpu")), VideoDecode::Gpu);
        assert_eq!(video_decode(Some("\tgpu \n")), VideoDecode::Gpu, "trimmed");
        for other in ["software", "", "GPU", "gpu2", "true", "hardware"] {
            assert_eq!(
                video_decode(Some(other)),
                VideoDecode::Software,
                "{other:?}"
            );
        }
        assert_eq!(VideoDecode::Software.as_str(), "software");
        assert_eq!(VideoDecode::Gpu.as_str(), "gpu");
        assert_eq!(VideoDecode::default(), VideoDecode::Software);
    }
}
