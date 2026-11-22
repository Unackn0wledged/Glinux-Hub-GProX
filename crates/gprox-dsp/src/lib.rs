//! DSP blocks for gprox and the interface the PipeWire nodes drive.
//!
//! Real-time contract for everything called from [`Processor::process`]:
//! no allocation, no locks, no syscalls, no logging. Parameter
//! changes arrive through lock-free structures ([`shared`]).
//!
//! Blocks:
//! * [`hp_eq`]: G HUB headphone EQ (12-band biquad cascade), built from
//!   [`biquad`] (coefficient design) and [`kernel`] (swappable cascade kernel;
//!   default: G HUB's IPP `f32` kernel, bit-exact).
//! * [`bluevoice`]: Blue VO!CE capture chain: de-popper, NR,
//!   expander, de-esser, voice EQ, compressor, limiter, volume/balance.
//!   Shared pieces: [`smoothing`] (one-pole, two-speed, enable cross-fade),
//!   [`lookahead`] (peak-hold detector, sub-block look-ahead delay with gain
//!   ramp), [`rbj`] (RBJ designs and the DF2/TDF2 section kernels).
//! * [`nr`]: Blue VO!CE noise reduction (32-band spectral gain).
//! * [`silence`]: mic activity meter on the raw mic (for the mute status).

pub mod biquad;
pub mod bluevoice;
pub mod hp_eq;
pub mod kernel;
pub mod lookahead;
pub mod nr;
pub mod rbj;
pub mod shared;
pub mod silence;
pub mod smoothing;

pub use hp_eq::{
    EqMode, HeadphoneEq, HpEqHandle, HpEqSettings, RampBlock, cascade_magnitude_db,
    steady_state_taps,
};
pub use nr::{NoiseReduction, NrHandle, NrSettings, NrTiming};

/// Upper bound on channels per node. Fixed so the RT path can use stack arrays.
pub const MAX_CHANNELS: usize = 8;

/// One planar block of audio: `input[ch][frame]` → `output[ch][frame]`.
///
/// Called on the PipeWire data thread. All slices in one call have the same
/// length (`frames`); channel counts equal what the node was configured with.
pub trait Processor: 'static {
    /// Prepare for a sample rate and maximum block size. Called off the RT
    /// thread, before the first `process` and whenever the format changes.
    fn prepare(&mut self, _sample_rate: u32, _max_frames: usize) {}

    /// Process one block. Must be real-time safe.
    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]);
}

/// Copies input to output channel by channel. Extra output channels are zeroed.
#[derive(Debug, Default, Clone, Copy)]
pub struct Passthrough;

impl Processor for Passthrough {
    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        for (ch, out) in output.iter_mut().enumerate() {
            match input.get(ch) {
                Some(inp) => {
                    let n = out.len().min(inp.len());
                    out[..n].copy_from_slice(&inp[..n]);
                    out[n..].fill(0.0);
                }
                None => out.fill(0.0),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_is_bit_exact() {
        let l: Vec<f32> = (0..256).map(|i| (i as f32 * 0.01).sin()).collect();
        let r: Vec<f32> = (0..256).map(|i| (i as f32 * 0.03).cos()).collect();
        let mut ol = vec![1.0; 256];
        let mut or = vec![1.0; 256];
        Passthrough.process(&[&l, &r], &mut [&mut ol, &mut or]);
        assert_eq!(ol, l);
        assert_eq!(or, r);
    }

    #[test]
    fn passthrough_zeroes_missing_channels_and_tail() {
        let m = [0.5_f32; 4];
        let mut a = [1.0_f32; 6];
        let mut b = [1.0_f32; 6];
        Passthrough.process(&[&m], &mut [&mut a, &mut b]);
        assert_eq!(a, [0.5, 0.5, 0.5, 0.5, 0.0, 0.0]);
        assert_eq!(b, [0.0; 6]);
    }
}
