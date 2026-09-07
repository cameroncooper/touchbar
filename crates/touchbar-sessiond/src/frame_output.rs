use std::{
    fs::OpenOptions,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context as _, Result};
use memmap2::{MmapMut, MmapOptions};
use touchbar_protocol::{
    FRAME_STREAM_ACTIVE_SLOT_OFFSET, FRAME_STREAM_FLAGS_OFFSET, FRAME_STREAM_HEADER_SIZE,
    FRAME_STREAM_MAGIC, FRAME_STREAM_SEQUENCE_OFFSET, FRAME_STREAM_SLOT_COUNT,
};

pub struct FramePublisher {
    map: MmapMut,
    frame_bytes: usize,
    sequence: u64,
}

impl FramePublisher {
    pub fn new(path: &Path, width: u32, height: u32) -> Result<Self> {
        let stride = width as usize * 4;
        let frame_bytes = stride * height as usize;
        let stream_bytes = FRAME_STREAM_HEADER_SIZE + frame_bytes * FRAME_STREAM_SLOT_COUNT;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .with_context(|| format!("create frame output {}", path.display()))?;
        file.set_len(stream_bytes as u64)
            .context("size frame output")?;
        // SAFETY: this process owns the newly truncated file and keeps the
        // mapping alive inside FramePublisher.
        let mut map = unsafe { MmapOptions::new().map_mut(&file) }.context("map frame output")?;
        map.fill(0);
        map[0..8].copy_from_slice(&FRAME_STREAM_MAGIC);
        map[8..12].copy_from_slice(&width.to_le_bytes());
        map[12..16].copy_from_slice(&height.to_le_bytes());
        map[16..20].copy_from_slice(&(stride as u32).to_le_bytes());
        map[20..24].copy_from_slice(&(FRAME_STREAM_SLOT_COUNT as u32).to_le_bytes());
        // No flags: the composited scene is top-down. GL's bottom-up origin
        // never enters this path, because the compositor's blit chain maps
        // framebuffer row 0 to texel row 0 at every hop and so carries the
        // client buffer's row order straight through the readback.
        map[FRAME_STREAM_FLAGS_OFFSET..FRAME_STREAM_FLAGS_OFFSET + 4]
            .copy_from_slice(&0_u32.to_le_bytes());

        println!(
            "frame-output={} size={}x{} slots={FRAME_STREAM_SLOT_COUNT}",
            path.display(),
            width,
            height
        );
        Ok(Self {
            map,
            frame_bytes,
            sequence: 0,
        })
    }

    pub fn publish(&mut self, pixels: &[u8]) {
        debug_assert_eq!(pixels.len(), self.frame_bytes);
        let next_sequence = self.sequence + 2;
        let slot = (next_sequence as usize / 2) % FRAME_STREAM_SLOT_COUNT;

        // Odd means a write is in progress. The aligned atomic lives entirely
        // inside the fixed header and does not overlap the mutable pixel slots.
        self.sequence_atomic()
            .store(next_sequence - 1, Ordering::Release);
        let start = FRAME_STREAM_HEADER_SIZE + slot * self.frame_bytes;
        self.map[start..start + self.frame_bytes].copy_from_slice(pixels);
        self.map[FRAME_STREAM_ACTIVE_SLOT_OFFSET..FRAME_STREAM_ACTIVE_SLOT_OFFSET + 4]
            .copy_from_slice(&(slot as u32).to_le_bytes());
        self.sequence_atomic()
            .store(next_sequence, Ordering::Release);
        self.sequence = next_sequence;
    }

    fn sequence_atomic(&self) -> &AtomicU64 {
        // SAFETY: FRAME_STREAM_SEQUENCE_OFFSET is eight-byte aligned, the
        // mapping is page-aligned, and its lifetime contains this reference.
        unsafe {
            &*(self
                .map
                .as_ptr()
                .add(FRAME_STREAM_SEQUENCE_OFFSET)
                .cast::<AtomicU64>())
        }
    }
}
