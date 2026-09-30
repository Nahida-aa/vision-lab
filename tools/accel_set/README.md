# accel_set（加速度设置探测工具）

用 Rust `input` crate（libinput 绑定）枚举 seat0 的指针设备，并尝试把
`ydotoold virtual device` 的加速度档设为 `Flat`。

## 结论（已实测）

- ✅ 能以 `input` 组权限看到 ydotool 虚拟设备，且它支持 `Flat` 档；
- ✅ 在我们自己的 libinput 上下文里 `config_accel_set_profile(Flat)` 调用成功（同上下文读回变 `Flat`）；
- ❌ **但不影响 KWin 实际使用的上下文**：libinput 的加速度 filter 是 per-context 的，
  KWin 各自维护独立状态。set 后经 ydotool 发 `move_rel((900,0))`、KWin 读光标，实际
  偏移仍是 ~1057（非 900），外部 `libinput list-devices` 仍显示 `adaptive`。

→ 本工具证明：**用户态无法把 ydotool 虚拟指针对 KWin 的有效加速度关掉**（KWin/Xwayland 下）。
详见 `crates/screen_operator/docs/mousemove.md` 的「指针加速度」节。

## 运行

```bash
cargo run -p accel_set
```

需要当前用户在 `input` 组。
