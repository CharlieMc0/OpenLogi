//! `openlogi diag mouse-buttons` — probe HID++ `0x8100` `OnboardProfiles` and
//! `0x8110` `MouseButtonSpy`, the gaming-line feature pair G-series mice
//! (e.g. the G502 X PLUS) report instead of `0x1b04 ReprogControlsV4`.
//!
//! Never writes onboard-profile memory or the device's onboard/host mode.
//! `--watch` does start the device's `0x8110` spy stream (a device-state
//! change, not a memory write) to answer, against real hardware, the
//! questions button-remap support for these devices needs before it can be
//! designed: does the spy emit events without a mode switch, what's the
//! button-index-to-physical-button mapping, and which buttons also still
//! produce a native OS click alongside a spy event. `--watch` stops the spy
//! again on Enter or Ctrl-C, and on every error path; if the process is
//! killed harder than that (`kill -9`, a crash), the spy is left running on
//! the device until stopped again — `--stop` recovers from that.

use anyhow::{Context, Result, bail};
use clap::Args;
use openlogi_hid::{DeviceRoute, MouseButtonIndex, MouseButtonSpyEvent};

use crate::cmd::diag::select_device;

#[derive(Debug, Args)]
pub struct MouseButtonsArgs {
    /// Run against the device whose name contains this string
    /// (case-insensitive) instead of auto-selecting. Useful when several
    /// devices are paired (e.g. a mouse and a keyboard over Bluetooth).
    #[arg(long, value_name = "NAME")]
    pub device: Option<String>,

    /// Start the `0x8110` button spy and print each event live, until Enter
    /// or Ctrl-C, instead of the default one-shot descriptor/mode/count read.
    #[arg(long, conflicts_with = "stop")]
    pub watch: bool,

    /// Stop the `0x8110` button spy without starting or reading anything —
    /// recovery for a `--watch` run that was killed hard enough to skip its
    /// own stop-on-exit (a `kill -9`, a crash).
    #[arg(long, conflicts_with = "watch")]
    pub stop: bool,
}

pub async fn run(args: MouseButtonsArgs) -> Result<()> {
    // 0x8110 = MouseButtonSpy — the feature present on G-series gaming mice
    // that lack 0x1b04 ReprogControlsV4 (e.g. the G502 X PLUS).
    let (route, name) = select_device(args.device.as_deref(), &[0x8110]).await?;
    println!("device: {name} ({route})");

    if args.stop {
        return stop_only(&route).await;
    }

    match openlogi_hid::dump_onboard_profiles(&route).await {
        Ok(info) => {
            println!(
                "  0x8100 OnboardProfiles: mode={:?} profile_format=0x{:02x} \
                 button_count={} profiles={}+{} oob sectors={}x{}B",
                info.mode,
                info.description.profile_format_id,
                info.description.button_count,
                info.description.profile_count,
                info.description.profile_count_oob,
                info.description.sector_count,
                info.description.sector_size,
            );
        }
        Err(e) => println!("  0x8100 OnboardProfiles: not available ({e:#})"),
    }

    let spy_available = match openlogi_hid::dump_mouse_button_count(&route).await {
        Ok(count) => {
            println!("  0x8110 MouseButtonSpy: button_count={count}");
            true
        }
        Err(e) => {
            println!("  0x8110 MouseButtonSpy: not available ({e:#})");
            false
        }
    };

    if !args.watch {
        return Ok(());
    }
    if !spy_available {
        bail!("--watch requires HID++ 0x8110 MouseButtonSpy, which this device doesn't report");
    }
    watch(&route).await
}

/// Opens the spy and immediately stops it — recovery for `--stop`.
async fn stop_only(route: &DeviceRoute) -> Result<()> {
    let spy = openlogi_hid::open_mouse_button_spy(route)
        .await
        .context("open HID++ 0x8110 MouseButtonSpy")?;
    spy.stop_reporting()
        .await
        .context("stop HID++ 0x8110 MouseButtonSpy")?;
    println!("  0x8110 MouseButtonSpy: stopped");
    Ok(())
}

/// Streams `0x8110` button-state events until Enter or Ctrl-C, then stops the
/// spy — on every exit path, including an error mid-stream. A raw
/// `std::thread` reads the blocking stdin line so this needs no extra tokio
/// feature beyond what's already enabled (`sync`, `macros`, `signal`).
async fn watch(route: &DeviceRoute) -> Result<()> {
    let spy = openlogi_hid::open_mouse_button_spy(route)
        .await
        .context("open HID++ 0x8110 MouseButtonSpy")?;
    let events = spy.listen();
    spy.start_reporting()
        .await
        .context("start HID++ 0x8110 MouseButtonSpy")?;

    println!(
        "watching — press each physical button once, in isolation, then press Enter here \
         (or Ctrl-C) to stop"
    );

    let streamed = stream_until_stop(&events).await;

    let stopped = spy
        .stop_reporting()
        .await
        .context("stop HID++ 0x8110 MouseButtonSpy");

    streamed.and(stopped)
}

async fn stream_until_stop(events: &async_channel::Receiver<MouseButtonSpyEvent>) -> Result<()> {
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        let _ = stop_tx.send(());
    });

    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());

    loop {
        tokio::select! {
            event = events.recv() => {
                let Ok(MouseButtonSpyEvent::Buttons(mask)) = event else { break };
                let down: Vec<u8> = mask.pressed().map(MouseButtonIndex::get).collect();
                println!("  mask=0x{:04x}  down={down:?}", mask.bits());
            }
            _ = &mut stop_rx => break,
            _ = &mut ctrl_c => {
                println!("stopping spy");
                break;
            }
        }
    }

    Ok(())
}
