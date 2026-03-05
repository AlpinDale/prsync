use std::{
    fs::File,
    io::Write,
    path::Path,
};
#[cfg(not(unix))]
use std::io::Read;

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};

use super::protocol::DeltaOp;

pub fn apply_delta_ops(
    basis_path: &Path,
    output_path: &Path,
    ops: &[DeltaOp],
    block_size: u32,
    start_op_index: usize,
) -> Result<(u64, usize, u128)> {
    let mut basis = File::open(basis_path)
        .with_context(|| format!("open basis file: {}", basis_path.display()))?;
    let mut out = File::create(output_path)
        .with_context(|| format!("open delta output: {}", output_path.display()))?;
    let mut written = 0_u64;
    let mut md5_ctx = md5::Context::new();

    for (idx, op) in ops.iter().enumerate().skip(start_op_index) {
        match op {
            DeltaOp::Copy { block_index, len } => {
                let offset = (*block_index * block_size as u64) as usize;
                let mut tmp = vec![0_u8; *len as usize];
                read_exact_at(&mut basis, offset, &mut tmp)?;
                out.write_all(&tmp)?;
                md5_ctx.consume(&tmp);
                written += *len as u64;
            }
            DeltaOp::Literal { data_b64 } => {
                let buf = STANDARD
                    .decode(data_b64)
                    .map_err(|e| anyhow!("decode literal: {e}"))?;
                out.write_all(&buf)?;
                md5_ctx.consume(&buf);
                written += buf.len() as u64;
            }
        }
        if idx % 128 == 0 {
            out.flush()?;
        }
    }
    out.sync_all()?;
    let digest = u128::from_be_bytes(md5_ctx.compute().0);
    Ok((written, ops.len(), digest))
}

fn read_exact_at(file: &mut File, offset: usize, out: &mut [u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(out, offset as u64)
            .context("read basis block")?;
    }
    #[cfg(not(unix))]
    {
        use std::io::Seek;
        file.seek(std::io::SeekFrom::Start(offset as u64))?;
        file.read_exact(out)?;
    }
    Ok(())
}
