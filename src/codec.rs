use anyhow::{Context, Result, anyhow, bail};
use wasmi::{Caller, Engine, Extern, Linker, Memory, Module, Store, TypedFunc};

use crate::video::CompressedFrame;

const CODEC_WASM: &[u8] = include_bytes!("../third_party/aspeed_codec/decoder_wasm.wasm");
type DecodeParams = (i32, i32, i32, i32, i32, i32, i32, i32);

/// Thin Rust host for ASPEED's existing MPL-2.0 decoder WebAssembly module.
///
/// The output allocation is deliberately retained across calls: AST frames are
/// differential and the upstream decoder updates the previous RGBA framebuffer.
pub struct AspeedCodec {
    store: Store<()>,
    memory: Memory,
    malloc: TypedFunc<i32, i32>,
    free: TypedFunc<i32, ()>,
    decode: TypedFunc<DecodeParams, ()>,
    input_ptr: i32,
    input_capacity: usize,
    output_ptr: i32,
    output_capacity: usize,
}

impl AspeedCodec {
    pub fn new() -> Result<Self> {
        let engine = Engine::default();
        let module = Module::new(&engine, CODEC_WASM).context("parse ASPEED codec WASM")?;
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);

        // Emscripten imports this as a.a(requested_bytes). Its generated JS
        // grows the exported linear memory and returns 1 on success.
        linker
            .func_wrap(
                "a",
                "a",
                |mut caller: Caller<'_, ()>, requested: i32| -> i32 {
                    let Some(Extern::Memory(memory)) = caller.get_export("b") else {
                        return 0;
                    };
                    if requested <= 0 {
                        return 0;
                    }
                    let current = memory.data(&caller).len();
                    let requested = requested as usize;
                    if requested <= current {
                        return 1;
                    }
                    let extra = requested - current;
                    let pages = extra.div_ceil(65536);
                    let Ok(pages) = u32::try_from(pages) else {
                        return 0;
                    };
                    if memory.grow(&mut caller, pages).is_ok() {
                        1
                    } else {
                        0
                    }
                },
            )
            .context("define Emscripten memory growth import")?;

        let instance = linker
            .instantiate(&mut store, &module)
            .context("instantiate ASPEED codec")?
            .start(&mut store)
            .context("start ASPEED codec")?;
        let memory = instance
            .get_memory(&store, "b")
            .ok_or_else(|| anyhow!("ASPEED codec does not export memory 'b'"))?;
        let constructors = instance
            .get_typed_func::<(), ()>(&store, "c")
            .context("find codec constructors")?;
        let init = instance
            .get_typed_func::<(), ()>(&store, "d")
            .context("find codec init")?;
        let malloc = instance
            .get_typed_func::<i32, i32>(&store, "e")
            .context("find codec malloc")?;
        let decode = instance
            .get_typed_func::<DecodeParams, ()>(&store, "f")
            .context("find codec decode")?;
        let free = instance
            .get_typed_func::<i32, ()>(&store, "h")
            .context("find codec free")?;

        constructors
            .call(&mut store, ())
            .context("run codec constructors")?;
        init.call(&mut store, ())
            .context("initialize codec tables")?;

        Ok(Self {
            store,
            memory,
            malloc,
            free,
            decode,
            input_ptr: 0,
            input_capacity: 0,
            output_ptr: 0,
            output_capacity: 0,
        })
    }

    pub fn decode(&mut self, frame: &CompressedFrame) -> Result<Vec<u8>> {
        if frame.header.rc4_enabled {
            bail!("RC4-encrypted AST frames are not supported by the selected codec adapter");
        }
        let width = frame.header.source_width as usize;
        let height = frame.header.source_height as usize;
        let pixel_count = width
            .checked_mul(height)
            .ok_or_else(|| anyhow!("frame dimensions overflow"))?;
        let output_len = pixel_count
            .checked_mul(4)
            .ok_or_else(|| anyhow!("framebuffer size overflow"))?;

        self.ensure_allocations(frame.data.len() + 8, output_len)?;
        self.memory
            .write(&mut self.store, self.input_ptr as usize, &frame.data)
            .context("copy compressed frame into codec")?;
        self.memory
            .write(
                &mut self.store,
                self.input_ptr as usize + frame.data.len(),
                &[0; 8],
            )
            .context("pad compressed frame")?;

        self.decode
            .call(
                &mut self.store,
                (
                    self.input_ptr,
                    frame.data.len() as i32,
                    self.output_ptr,
                    width as i32,
                    height as i32,
                    frame.header.mode_420 as i32,
                    frame.header.jpeg_table_selector as i32,
                    frame.header.advance_table_selector as i32,
                ),
            )
            .context("ASPEED codec rejected frame")?;

        let mut rgba = vec![0_u8; output_len];
        self.memory
            .read(&self.store, self.output_ptr as usize, &mut rgba)
            .context("copy decoded framebuffer from codec")?;
        Ok(rgba)
    }

    fn ensure_allocations(&mut self, input_len: usize, output_len: usize) -> Result<()> {
        if input_len > self.input_capacity {
            if self.input_ptr != 0 {
                self.free
                    .call(&mut self.store, self.input_ptr)
                    .context("free old codec input")?;
            }
            self.input_ptr = self
                .malloc
                .call(&mut self.store, input_len as i32)
                .context("allocate codec input")?;
            if self.input_ptr == 0 {
                bail!("ASPEED codec input allocation failed");
            }
            self.input_capacity = input_len;
        }
        if output_len > self.output_capacity {
            if self.output_ptr != 0 {
                self.free
                    .call(&mut self.store, self.output_ptr)
                    .context("free old codec output")?;
            }
            self.output_ptr = self
                .malloc
                .call(&mut self.store, output_len as i32)
                .context("allocate codec output")?;
            if self.output_ptr == 0 {
                bail!("ASPEED codec output allocation failed");
            }
            // The upstream wrapper initializes a new differential framebuffer
            // to opaque white and then preserves it across later frames.
            let white = vec![0xff; output_len];
            self.memory
                .write(&mut self.store, self.output_ptr as usize, &white)
                .context("initialize codec framebuffer")?;
            self.output_capacity = output_len;
        }
        Ok(())
    }
}

impl Drop for AspeedCodec {
    fn drop(&mut self) {
        if self.input_ptr != 0 {
            let _ = self.free.call(&mut self.store, self.input_ptr);
        }
        if self.output_ptr != 0 {
            let _ = self.free.call(&mut self.store, self.output_ptr);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_codec_instantiates() {
        AspeedCodec::new().expect("bundled ASPEED codec should instantiate");
    }
}
