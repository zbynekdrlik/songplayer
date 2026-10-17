//! #282: the scaled half of `MediaFoundationVideoReader`, a child module of
//! `mf_reader.rs` (split out for the 1000-line cap; as a child it reaches
//! the reader's private fields and functions): the second source reader of
//! a scaled open and its check (`hw_decode::scaled_open_safe`), its INFO /
//! WARN lines, the first picture's re-read of the decoder's output, and the
//! tests' accessors. What the output type carries, and how the transform
//! chain is read, is `mf_scaled.rs`. Every decision is `crate::hw_decode`'s.

use std::path::Path;

use tracing::{info, warn};

use super::{MediaFoundationVideoReader, Opened};
use crate::error::DecoderError;
use crate::hw_decode::{VideoArea, padding_reaches_processor, scaled_open_safe};
use crate::video::hw_session::HwSession;
use crate::video::mf_scaled::{self, ScaledOpenFacts, ScaledOpenRefusal, ScaledType};

impl MediaFoundationVideoReader {
    /// #282: a second source reader on the file, with Media Foundation's
    /// video processor, negotiated at `shape`. Refused where the decoder's
    /// padding could reach the processor (`hw_decode::scaled_open_safe`,
    /// from the decoder's output type in the reader's chain): the processor
    /// would scale the padding rows (green) into the picture. An open that
    /// goes on logs one INFO with its facts ([`ScaledOpenFacts`]).
    pub(super) fn open_scaled(
        path: &Path,
        wide_path: &[u16],
        hw: Option<&HwSession>,
        shape: &ScaledType,
    ) -> Result<Opened, DecoderError> {
        if mf_scaled::refused_by_test() {
            return Err(DecoderError::NoStream(
                "video: a scaled open refused by a test".into(),
            ));
        }
        let reader = Self::source_reader(wide_path, hw, true)?;
        let mut opened = Self::configure(reader, hw, Some(shape.from), Some(shape))?;
        let chain = mf_scaled::read_chain(&opened.reader);
        let seen = chain
            .decoder
            .as_ref()
            .ok()
            .map(|out| (out.size, out.aperture));
        let decoder = match &chain.decoder {
            Ok(out) => format!("{}x{} aperture {:?}", out.size.0, out.size.1, out.aperture),
            Err(why) => format!("unreadable ({why})"),
        };
        if !scaled_open_safe(shape.from, seen, hw.is_some()) {
            return Err(DecoderError::NoStream(format!(
                "video: the decoder's padding could reach the video processor for a {}x{} \
                 stream (decoder output: {decoder})",
                shape.from.0, shape.from.1
            )));
        }
        let on = if hw.is_some() { "D3D11" } else { "software" };
        if chain.decoder.is_err() {
            // `scaled_open_safe` lets an unreadable chain through in software
            // and, on the D3D11 path, for a stream of whole 16-pixel blocks.
            warn!(
                file = %path.display(),
                decoder = %decoder,
                path = on,
                "mf_reader: the decoder's output type could not be read; the scaled open goes \
                 on (software: the decoder is taken to hand over the stream's own size; \
                 D3D11: a stream of whole 16-pixel blocks, which NVDEC does not pad)"
            );
        }
        let current = mf_scaled::current_type_facts(&opened.reader);
        let facts = ScaledOpenFacts {
            chain: chain.stages,
            native_aperture: shape.native_aperture,
            native_colour: shape.colour,
            out_colour: current.map(|(colour, _)| colour),
            out_pixel_aspect: current.and_then(|(_, ratio)| ratio),
            decoder: chain.decoder,
            decoder_at_first_picture: None,
            first_picture_padding_in_reach: None,
        };
        info!(
            file = %path.display(),
            from = %format!("{}x{}", shape.from.0, shape.from.1),
            to = %format!("{}x{}", shape.size.0, shape.size.1),
            pixel_aspect = ?shape.pixel_aspect,
            pixel_aspect_out = ?facts.out_pixel_aspect,
            decoder = ?facts.decoder,
            chain = ?facts.chain,
            native_aperture = ?facts.native_aperture,
            colour_native = ?facts.native_colour,
            colour_out = ?facts.out_colour,
            colour_decoder = ?facts.decoder.as_ref().ok().map(|out| out.colour),
            path = on,
            "mf_reader: opened scaled to the decode cap (#282); the decoder's own path is not visible behind the video processor"
        );
        opened.scaled_facts = Some(facts);
        opened.decoder_mft = chain.decoder_mft;
        Ok(opened)
    }

    /// #282 follow-up: at the first picture of a scaled open, read the
    /// decoder's output type again, once (then the decoder MFT is let go).
    /// On the D3D11 path NVDEC may learn its padded size only at the first
    /// frame, after the open's check, so one INFO line names what the
    /// decoder hands the processor now and whether that padding reaches the
    /// processor (`hw_decode::padding_reaches_processor` against the
    /// stream's own size: where the green strip at the picture's bottom is
    /// expected). Both are kept in the facts for the tests.
    pub(super) fn reread_decoder_output(&mut self) {
        let (Some(decoder), Some(native)) = (self.decoder_mft.take(), self.scaled_from) else {
            return;
        };
        let again = mf_scaled::decoder_output_of(&decoder);
        let in_reach = again
            .as_ref()
            .is_ok_and(|out| padding_reaches_processor(out.size, out.aperture, native));
        info!(
            file = %self.path.display(),
            at_open = ?self.scaled_facts.as_ref().map(|facts| &facts.decoder),
            at_first_picture = ?again,
            padding_in_reach = in_reach,
            "mf_reader: the decoder's output at the first scaled picture (#282)"
        );
        if let Some(facts) = self.scaled_facts.as_mut() {
            facts.decoder_at_first_picture = Some(again);
            facts.first_picture_padding_in_reach = Some(in_reach);
        }
    }

    /// Make the next scaled open in this process fail as a refusal by Media
    /// Foundation would, so that reader opens its file at its own size: the
    /// only way CI reaches the refusal path (Media Foundation takes the
    /// scaled type there). The refusal stays armed until a scaled open uses
    /// it or the returned guard drops, so a test that panics first never
    /// refuses a later test's open. For tests, not production.
    #[doc(hidden)]
    pub fn refuse_next_scaled_open_for_test() -> ScaledOpenRefusal {
        mf_scaled::refuse_next()
    }

    /// #282: the stream's own size when this file was opened scaled to the
    /// decode cap (the reader's `width` / `height` are then the scaled
    /// size); `None` at its own size.
    pub fn scaled_from(&self) -> Option<(u32, u32)> {
        self.scaled_from
    }

    /// #282: what the decoder handed the video processor at a scaled open:
    /// its output type's size and display aperture (`None` unscaled, or
    /// when the chain could not be read). For tests and the box check, not
    /// production.
    #[doc(hidden)]
    pub fn scaled_decoder_output(&self) -> Option<((u32, u32), Option<VideoArea>)> {
        let facts = self.scaled_facts.as_ref()?;
        let out = facts.decoder.as_ref().ok()?;
        Some((out.size, out.aperture))
    }

    /// #282 follow-up: what this file's scaled open read of Media
    /// Foundation (`None` at its own size). For tests and the box check,
    /// not production.
    #[doc(hidden)]
    pub fn scaled_open_facts(&self) -> Option<&ScaledOpenFacts> {
        self.scaled_facts.as_ref()
    }
}
