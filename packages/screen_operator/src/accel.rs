//! 通过 KWin 设备级 D-Bus 把 ydotool 虚拟指针的加速度设成 flat。
//!
//! 背景与实证见 `docs/mousemove.md` 的「指针加速度」节。要点：
//! - KWin 在 Wayland 下用自己的 libinput 后端读输入设备，对每个设备维护独立的加速度状态
//!   （KWin 源码 `src/backends/libinput/device.cpp`）。
//! - 每个输入设备在 `/org/kde/KWin/InputDevice/<sysName>` 导出所有 Q_PROPERTY，其中
//!   `pointerAccelerationProfileFlat` 是 **readwrite**——这才是「正确的一侧」context
//!   （外部 xinput / libinput quirk / 独立 libinput context 都动不到这里）。
//! - 设 true 后实测 `move_rel((900,0))` 实际偏移从 1041 降到 900（误差 0），加速度确为过冲源。
//! - 该属性只作用于 ydotool 的 uinput 虚拟设备，与真实鼠标无关，**无需恢复**，常驻 flat 即可。
//!   但 ydotoold 重启会把设备重置回 adaptive，故采用「幂等确保」而非一次性设置。

use anyhow::{Context, Result};

const KWIN_DEST: &str = "org.kde.KWin";
const INPUT_DEVICE_MGR: &str = "/org/kde/KWin/InputDevice";
const YDOTOOL_DEVICE_NAME: &str = "ydotoold virtual device";

/// 在 KWin 的 InputDevice 管理器下找到 ydotool 虚拟设备对应的 D-Bus 路径。
///
/// `/org/kde/KWin/InputDevice` 管理器节点**不实现 ObjectManager**（实测
/// `GetManagedObjects` 报 UnknownInterface），但对其 Introspect 会列出所有子节点
/// （`<node name="eventN"/>`）。故这里 Introspect 管理器、解析子节点名，逐个拼路径
/// 读 `name` 属性匹配 `ydotoold virtual device`。返回形如
/// `/org/kde/KWin/InputDevice/event16`。
fn find_ydotool_device_path(conn: &dbus::blocking::Connection) -> Result<Option<String>> {
    use dbus::blocking::stdintf::org_freedesktop_dbus::Introspectable;
    use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;

    let mgr = conn.with_proxy(
        KWIN_DEST,
        INPUT_DEVICE_MGR,
        std::time::Duration::from_secs(2),
    );
    let xml = mgr
        .introspect()
        .context("KWin InputDevice 管理器 Introspect 失败")?;

    // 解析所有 <node name="..."/>，得到各设备的 sysName。
    // 注意：XML 开头（<!DOCTYPE ...>）也可能含引号，需用「无空白、无尖括号」过滤出真节点名。
    for cap in xml.split("<node name=\"") {
        let Some(sys_name) = cap.split('\"').next() else {
            continue;
        };
        if sys_name.is_empty()
            || sys_name.contains(' ')
            || sys_name.contains('<')
            || sys_name.contains('>')
        {
            continue;
        }
        let path = format!("{INPUT_DEVICE_MGR}/{sys_name}");
        let dev = conn.with_proxy(KWIN_DEST, path, std::time::Duration::from_secs(2));
        let name: Result<String, _> = dev.get("org.kde.KWin.InputDevice", "name");
        if let Ok(name) = name {
            if name == YDOTOOL_DEVICE_NAME {
                return Ok(Some(format!("{INPUT_DEVICE_MGR}/{sys_name}")));
            }
        }
    }
    Ok(None)
}

/// 幂等确保 ydotool 虚拟设备的加速度为 flat。
///
/// - 找不到 KWin / 找不到设备 / 任何 D-Bus 错误都**静默返回 Ok**：加速度未关只意味着
///   移动闭环效率略低（每步指令≠实际位移），不影响最终正确性——不应因此阻断移动。
/// - 已是 flat 则不动；不是才设 true（幂等）。
pub fn ensure_ydotool_flat() -> Result<()> {
    let conn = match dbus::blocking::Connection::new_session() {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("accel: 无法连接 session D-Bus（{e}），跳过 flat 设置");
            return Ok(());
        }
    };

    let path = match find_ydotool_device_path(&conn)? {
        Some(p) => p,
        None => {
            tracing::debug!("accel: 未找到 '{YDOTOOL_DEVICE_NAME}' 设备，跳过 flat 设置");
            return Ok(());
        }
    };

    use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
    let dev = conn.with_proxy(KWIN_DEST, path, std::time::Duration::from_secs(2));

    let is_flat: bool = match dev.get("org.kde.KWin.InputDevice", "pointerAccelerationProfileFlat")
    {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("accel: 读 pointerAccelerationProfileFlat 失败（{e}），跳过");
            return Ok(());
        }
    };

    if is_flat {
        tracing::debug!("accel: ydotool 虚拟设备已是 flat，无需设置");
        return Ok(());
    }

    dev.set(
        "org.kde.KWin.InputDevice",
        "pointerAccelerationProfileFlat",
        true,
    )
    .context("设置 pointerAccelerationProfileFlat=true 失败")?;

    tracing::debug!("accel: 已将 ydotool 虚拟设备加速度设为 flat");
    Ok(())
}
