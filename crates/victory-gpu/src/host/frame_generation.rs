//! Optional GPU frame extrapolation from real SDR presents.
use super::{Host, HostList, Presenter, transition};
use std::sync::{OnceLock, mpsc};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HANDLE, S_OK};
use windows::Win32::Graphics::Direct3D::{Fxc::D3DCompile, ID3DBlob};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::{Common::*, DXGI_PRESENT_DO_NOT_WAIT};
use windows::core::{Error, Interface, PCSTR, Result, s};

const BLOCK: u32 = 24;
const SOURCE: &str = include_str!("frame_generation.hlsl");
const READ: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
const OUTPUT_FORMAT: DXGI_FORMAT = DXGI_FORMAT_R8G8B8A8_UNORM;

pub(super) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_FRAME_GENERATION").is_some_and(|v| v == "1"))
}

pub(super) fn supports(format: DXGI_FORMAT) -> bool {
    format == OUTPUT_FORMAT
}

#[derive(Clone, Copy)]
struct Tick {
    frame: u64,
    due: Instant,
}

fn halfway(previous: Instant, now: Instant) -> Option<Instant> {
    let interval = now.saturating_duration_since(previous);
    (Duration::from_millis(55)..=Duration::from_millis(120))
        .contains(&interval)
        .then(|| now + interval / 2)
}

fn schedule(tick: Tick) {
    static WORK: OnceLock<Option<mpsc::SyncSender<Tick>>> = OnceLock::new();
    let tx = WORK.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel::<Tick>(1);
        std::thread::Builder::new()
            .name("victory-frame-generation".into())
            .spawn(move || {
                while let Ok(mut next) = rx.recv() {
                    loop {
                        let timeout = next.due.saturating_duration_since(Instant::now());
                        match rx.recv_timeout(timeout) {
                            Ok(newer) => next = newer,
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                if Instant::now() <= next.due + Duration::from_millis(8)
                                    && let Some(host) = super::host()
                                    && let Ok(mut presenter) = host.presenter.try_lock()
                                    && let Some(p) = presenter.as_mut()
                                {
                                    present_generated(host, p, next.frame);
                                }
                                break;
                            }
                            Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        }
                    }
                }
            })
            .ok()
            .map(|_| tx)
    });
    if let Some(tx) = tx {
        let _ = tx.try_send(tick);
    }
}

struct Compute {
    root: ID3D12RootSignature,
    pipeline: ID3D12PipelineState,
}

fn shader(entry: PCSTR) -> Result<ID3DBlob> {
    let (mut code, mut errors): (Option<ID3DBlob>, Option<ID3DBlob>) = (None, None);
    // SAFETY: the source and entry name live through this call.
    let result = unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            s!("frame_generation"),
            None,
            None,
            entry,
            s!("cs_5_1"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Some(errors) = errors {
        // SAFETY: the compiler owns the bytes for the lifetime of the blob.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                errors.GetBufferPointer().cast::<u8>(),
                errors.GetBufferSize(),
            )
        };
        tracing::warn!(message = %String::from_utf8_lossy(bytes), "frame generation shader compiler");
    }
    result?;
    code.ok_or_else(Error::empty)
}

impl Compute {
    fn new(device: &ID3D12Device, entry: PCSTR) -> Result<Self> {
        let code = shader(entry)?;
        // SAFETY: the shader blob stays live, and the descriptor borrows the root.
        unsafe {
            let bytes = std::slice::from_raw_parts(
                code.GetBufferPointer().cast::<u8>(),
                code.GetBufferSize(),
            );
            let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
            let pipeline =
                device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: code.GetBufferPointer(),
                        BytecodeLength: code.GetBufferSize(),
                    },
                    ..Default::default()
                })?;
            Ok(Self { root, pipeline })
        }
    }
}

fn texture(
    device: &ID3D12Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    state: D3D12_RESOURCE_STATES,
    writable: bool,
) -> Result<ID3D12Resource> {
    let mut resource = None;
    // SAFETY: a 2D single-sample texture and a valid output pointer.
    unsafe {
        device.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            },
            D3D12_HEAP_FLAG_NONE,
            &D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: u64::from(width),
                Height: height,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Flags: if writable {
                    D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
                } else {
                    D3D12_RESOURCE_FLAG_NONE
                },
                ..Default::default()
            },
            state,
            None,
            &mut resource,
        )?;
    }
    resource.ok_or_else(Error::empty)
}

pub(super) struct Generator {
    history: [ID3D12Resource; 2],
    motion: ID3D12Resource,
    output: ID3D12Resource,
    views: ID3D12DescriptorHeap,
    stride: u64,
    estimate: Compute,
    extrapolate: Compute,
    copy: HostList,
    complete: ID3D12Fence,
    sent: u64,
    unavailable: bool,
    newest: usize,
    last: Option<(u64, Instant)>,
    generated: u64,
    width: u32,
    height: u32,
}

// SAFETY: D3D12 handles are free-threaded, and the presenter lock protects this state.
unsafe impl Send for Generator {}

impl Generator {
    pub(super) fn new(host: &Host, width: u32, height: u32) -> Result<Self> {
        let d = &host.device;
        let history = [
            texture(d, width, height, OUTPUT_FORMAT, READ, false)?,
            texture(d, width, height, OUTPUT_FORMAT, READ, false)?,
        ];
        let motion = texture(
            d,
            width.div_ceil(BLOCK),
            height.div_ceil(BLOCK),
            DXGI_FORMAT_R16G16B16A16_FLOAT,
            READ,
            true,
        )?;
        let output = texture(
            d,
            width,
            height,
            OUTPUT_FORMAT,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            true,
        )?;
        let (estimate, extrapolate) = (
            Compute::new(d, s!("estimate"))?,
            Compute::new(d, s!("extrapolate"))?,
        );
        let copy = host.new_list()?;
        copy.close();
        // SAFETY: immutable descriptor slots. Each history parity has its own SRVs.
        let (views, stride, complete) = unsafe {
            let views: ID3D12DescriptorHeap =
                d.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 10,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    ..Default::default()
                })?;
            let stride = u64::from(
                d.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV),
            );
            let start = views.GetCPUDescriptorHandleForHeapStart().ptr;
            for parity in 0..2 {
                let slot = |i: usize| D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: start + (parity * 5 + i) * stride as usize,
                };
                d.CreateShaderResourceView(Some(&history[parity]), None, slot(0));
                d.CreateShaderResourceView(Some(&history[parity ^ 1]), None, slot(1));
                d.CreateShaderResourceView(Some(&motion), None, slot(2));
                d.CreateUnorderedAccessView(Some(&motion), None::<&ID3D12Resource>, None, slot(3));
                d.CreateUnorderedAccessView(Some(&output), None::<&ID3D12Resource>, None, slot(4));
            }
            (views, stride, d.CreateFence(0, D3D12_FENCE_FLAG_NONE)?)
        };
        tracing::info!(width, height, "experimental 2x frame generation enabled");
        Ok(Self {
            history,
            motion,
            output,
            views,
            stride,
            estimate,
            extrapolate,
            copy,
            complete,
            sent: 0,
            unavailable: false,
            newest: 0,
            last: None,
            generated: 0,
            width,
            height,
        })
    }

    /// Copies the actual composed SDR image without modifying it.
    pub(super) fn capture(&self, copy: &HostList, back: &ID3D12Resource) {
        let to = &self.history[self.newest ^ 1];
        copy.keep(to);
        // SAFETY: the presenter's open copy list owns the next history write.
        unsafe {
            copy.list.ResourceBarrier(&[
                transition(
                    back,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                ),
                transition(to, READ, D3D12_RESOURCE_STATE_COPY_DEST),
            ]);
            copy.list.CopyResource(to, back);
            copy.list.ResourceBarrier(&[
                transition(to, D3D12_RESOURCE_STATE_COPY_DEST, READ),
                transition(
                    back,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                ),
            ]);
        }
    }

    pub(super) fn presented(&mut self, frame: u64, success: bool) {
        if !success || self.unavailable {
            self.last = None;
            return;
        }
        self.newest ^= 1;
        let now = Instant::now();
        if let Some((before, at)) = self.last.replace((frame, now))
            && before.checked_add(1) == Some(frame)
            && let Some(due) = halfway(at, now)
        {
            schedule(Tick { frame, due });
        }
    }

    /// Wait only before destroying resized swap-chain buffers.
    pub(super) fn finish(&self) -> Result<()> {
        if self.sent != 0 {
            // SAFETY: a null event requests a synchronous wait for our own fence.
            unsafe {
                self.complete
                    .SetEventOnCompletion(self.sent, HANDLE::default())?
            };
        }
        Ok(())
    }

    fn emit(&mut self, host: &Host, back: &ID3D12Resource) -> Result<()> {
        let copy = &self.copy;
        copy.reset();
        let list = &copy.list;
        // SAFETY: the previous use of this list has completed. All textures,
        // descriptor sets and pipelines are owned by the live generator.
        unsafe {
            let base = self.views.GetGPUDescriptorHandleForHeapStart().ptr
                + 5 * self.newest as u64 * self.stride;
            let srv = D3D12_GPU_DESCRIPTOR_HANDLE { ptr: base };
            let uav = |index| D3D12_GPU_DESCRIPTOR_HANDLE {
                ptr: base + index * self.stride,
            };
            list.SetDescriptorHeaps(&[Some(self.views.clone())]);
            list.ResourceBarrier(&[transition(
                &self.motion,
                READ,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            )]);
            list.SetComputeRootSignature(&self.estimate.root);
            list.SetPipelineState(&self.estimate.pipeline);
            list.SetComputeRootDescriptorTable(0, srv);
            list.SetComputeRootDescriptorTable(1, uav(3));
            list.Dispatch(
                self.width.div_ceil(BLOCK).div_ceil(8),
                self.height.div_ceil(BLOCK).div_ceil(8),
                1,
            );
            list.ResourceBarrier(&[
                transition(&self.motion, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, READ),
                transition(
                    &self.output,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                ),
            ]);
            list.SetComputeRootSignature(&self.extrapolate.root);
            list.SetPipelineState(&self.extrapolate.pipeline);
            list.SetComputeRootDescriptorTable(0, srv);
            list.SetComputeRootDescriptorTable(1, uav(4));
            list.Dispatch(self.width.div_ceil(8), self.height.div_ceil(8), 1);
            list.ResourceBarrier(&[
                transition(
                    &self.output,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                ),
                transition(
                    back,
                    D3D12_RESOURCE_STATE_PRESENT,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                ),
            ]);
            list.CopyResource(back, &self.output);
            list.ResourceBarrier(&[transition(
                back,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_PRESENT,
            )]);
            copy.close();
            let command: ID3D12CommandList = list.cast()?;
            host.queue.ExecuteCommandLists(&[Some(command)]);
            let next = self.sent + 1;
            host.queue.Signal(&self.complete, next)?;
            self.sent = next;
        }
        Ok(())
    }
}

fn present_generated(host: &Host, p: &mut Presenter, frame: u64) {
    let Some(generator) = p.frame_generation.as_mut() else {
        return;
    };
    // SAFETY: swap-chain access is serialized with the real presenter.
    unsafe {
        if generator.unavailable
            || generator.last.is_none_or(|(last, _)| last != frame)
            || generator.complete.GetCompletedValue() < generator.sent
        {
            return;
        }
        let Ok(back) = p
            .swap_chain
            .GetBuffer::<ID3D12Resource>(p.swap_chain.GetCurrentBackBufferIndex())
        else {
            return;
        };
        if let Err(error) = generator.emit(host, &back) {
            generator.unavailable = true;
            tracing::warn!(%error, "frame generation stopped");
            return;
        }
        if p.swap_chain.Present(0, DXGI_PRESENT_DO_NOT_WAIT) == S_OK {
            generator.generated += 1;
            if generator.generated == 1 || generator.generated.is_multiple_of(60) {
                tracing::info!(
                    frame,
                    generated = generator.generated,
                    "generated frame presented"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_both_gpu_shaders() {
        shader(s!("estimate")).unwrap();
        shader(s!("extrapolate")).unwrap();
    }

    #[test]
    fn schedules_halfway_and_skips_fast_frames_and_stalls() {
        let previous = Instant::now();
        let now = previous + Duration::from_millis(66);
        assert_eq!(
            halfway(previous, now),
            Some(now + Duration::from_millis(33))
        );
        assert_eq!(
            halfway(previous, previous + Duration::from_millis(33)),
            None
        );
        assert_eq!(
            halfway(previous, previous + Duration::from_millis(300)),
            None
        );
    }
}
