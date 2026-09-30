//! 用 rust `input` crate(libinput 绑定)枚举 seat0 的指针设备,
//! 打印 ydotool 虚拟设备的加速度档位与支持档 —— 仅探测,不改设置。
//!
//! 目的:确认我们的 Rust 程序能以 `input` 组权限看到 ydotool 虚拟设备、
//! 且它支持 `Flat` 档。这是后续 `config_accel_set_profile(Flat)` 的生死线探测。
//!
//! 用法:
//!   cargo run -p accel_set
//! 需用户在 `input` 组(否则 open_restricted 打开 /dev/input/event* 会失败)。

use input::event::EventTrait;
use input::{AccelProfile, Libinput, LibinputInterface};
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use libc::{O_ACCMODE, O_RDONLY, O_RDWR, O_WRONLY};

struct Interface;

impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<std::os::fd::OwnedFd, i32> {
        OpenOptions::new()
            .custom_flags(flags)
            .read((flags & O_ACCMODE == O_RDONLY) | (flags & O_ACCMODE == O_RDWR))
            .write((flags & O_ACCMODE == O_WRONLY) | (flags & O_ACCMODE == O_RDWR))
            .open(path)
            .map(|file| file.into())
            .map_err(|err| err.raw_os_error().unwrap_or(-1))
    }
    fn close_restricted(&mut self, fd: std::os::fd::OwnedFd) {
        drop(fd);
    }
}

fn main() -> anyhow::Result<()> {
    let mut libinput = Libinput::new_with_udev(Interface);
    libinput
        .udev_assign_seat("seat0")
        .map_err(|_| anyhow::anyhow!("udev_assign_seat(\"seat0\") 失败:可能不在 seat0 会话"))?;

    // 先 dispatch 一轮,让 libinput 枚举到设备。
    libinput.dispatch()?;

    println!("=== seat0 下所有设备的加速度信息 ===");
    let mut found_ydotool = false;
    for ev in &mut libinput {
        // 只关心设备类事件(设备新增/移除/其他设备事件),从中取关联 Device。
        if !matches!(ev, input::Event::Device(_)) {
            continue;
        }
        let mut dev = ev.device();
        let name = dev.name().to_string();
        let is_pointer = dev.has_capability(input::DeviceCapability::Pointer);
        let cur = dev.config_accel_profile();
        let supported = dev.config_accel_profiles();

        if name.contains("ydotoold") {
            found_ydotool = true;
        }

        println!(
            "设备: {:?} | pointer={} | 当前档={:?} | 支持档={:?}",
            name, is_pointer, cur, supported
        );

        if name.contains("ydotoold") {
            println!(
                "  ↑ ydotool 虚拟设备: set 前当前档={:?}, 支持 Flat={}",
                cur,
                supported.contains(&AccelProfile::Flat)
            );
            // Phase 2: 真正设为 Flat,并在同上下文内立即读回确认。
            if supported.contains(&AccelProfile::Flat) {
                match dev.config_accel_set_profile(AccelProfile::Flat) {
                    Ok(()) => {
                        let after = dev.config_accel_profile();
                        println!(
                            "    ✓ set_profile(Flat) 请求已发; 同上下文读回当前档={:?} (若变 Flat=本上下文生效)",
                            after
                        );
                    }
                    Err(e) => println!("    ✗ set_profile(Flat) 失败: {:?}", e),
                }
            }
        }
    }

    if !found_ydotool {
        println!("⚠ 未枚举到名称含 'ydotoold' 的设备 —— 说明本程序看不到它(权限/独占/seat 问题)。");
    } else {
        println!("✓ 看到 ydotool 虚拟设备,且上述支持档含 Flat 即可进入 set 阶段。");
    }

    Ok(())
}
