use anyhow::Result;
use std::sync::Arc;
use crate::opencl::VRamBuffer;

/// Minimal block backend abstraction shared by different frontends (NBD, ublk)
pub trait BlockBackend: Send + Sync {
    fn size(&self) -> u64;
    fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<()>;
    fn write_at(&self, offset: u64, src: &[u8]) -> Result<()>;
}

impl BlockBackend for VRamBuffer {
    fn size(&self) -> u64 {
        self.size() as u64
    }

    fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<()> {
        self.read(offset as usize, dst)
    }

    fn write_at(&self, offset: u64, src: &[u8]) -> Result<()> {
        self.write(offset as usize, src)
    }
}

impl<T> BlockBackend for Arc<T>
where
    T: BlockBackend + ?Sized,
{
    fn size(&self) -> u64 {
        (**self).size()
    }

    fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<()> {
        (**self).read_at(offset, dst)
    }

    fn write_at(&self, offset: u64, src: &[u8]) -> Result<()> {
        (**self).write_at(offset, src)
    }
}