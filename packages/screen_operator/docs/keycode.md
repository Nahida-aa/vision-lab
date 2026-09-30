# 键盘输入：ydotool `key` 只认数字 keycode

## 坑

`ydotool key` 子命令**只接受数字 keycode**（`strtol` 解析），**不接受 `KEY_*` 名字**。

直接传名字会被当成 `0`（`KEY_RESERVED`）静默失效——命令返回 0、看似成功，但
什么都没发生：

```bash
# ❌ 错误：KEY_ENTER 不是数字，strtol 解析为 0 → 发 keycode 0 = 空操作
ydotool key KEY_ENTER
ydotool key KEY_LEFTCTRL:1 KEY_S:1 KEY_S:0 KEY_LEFTCTRL:0

# ✅ 正确：用 Linux input-event-codes.h 的数字码
#   Enter=28, LeftCtrl=29, S=31, A=30, End=107 ...
ydotool key 28:1 28:0
ydotool key 29:1 31:1 31:0 29:0      # Ctrl+S
```

对照：`ydotool type` 走的是 ASCII→keycode 映射表，所以 `ydotool type "abc"`
能正常输入——这也是为什么"打字能进、按键没反应"会让人误以为是 Wayland/KWin 的
修饰键限制，其实只是参数格式错了。

## 本 crate 的处理

`screen_operator` 已在内部解决此事：

- `key` / `key_down` / `key_up` / `combo` 接受 `KEY_*` 名字（大小写不敏感，如
  `"KEY_ENTER"`、`"KEY_LEFTCTRL"`），经 [`KEYCODES`](../src/lib.rs) 表（编译期
  phf 哈希）翻译成数字码后再发给 ydotool。
- 同时也接受纯数字：`"31"`（十进制）或 `"0x1F"`（十六进制），以及 `key_code(31)`。
- 未列出的特殊键可走 `key_code(数字)` 直发。

所以调用方**无需记忆数字码**，直接用直观名字即可：

```rust
use screen_operator::{ScreenOperator, MouseButton};
let op = ScreenOperator::new();
op.key("KEY_ENTER").unwrap();
op.combo(&["KEY_LEFTCTRL", "KEY_S"]).unwrap();  // Ctrl+S
```

常用 keycode 速查（来源 `linux/input-event-codes.h`）：

| 名字 | 码 | 名字 | 码 |
|------|----|------|----|
| KEY_LEFTCTRL | 29 | KEY_RIGHTCTRL | 97 |
| KEY_LEFTSHIFT | 42 | KEY_LEFTALT | 56 |
| KEY_ENTER | 28 | KEY_ESC | 1 |
| KEY_A..Z | 30..44 | KEY_0..9 | 11..2* |
| KEY_F1..F12 | 59..88 | KEY_END | 107 |
| KEY_HOME | 102 | KEY_LEFT/RIGHT/UP/DOWN | 105/106/103/108 |

> `KEYCODES` 表只覆盖常用键，缺的键请补到表里或走 `key_code(数字)`。

## 参考

- ydotool 源码 `Client/tool_key.c`：`kc = strtol(pstr, NULL, 10)`，非数字值
  "only cause a delay"。
- ydotool 源码 `Client/tool_type.c`：`ascii2keycode_map[]` 自带 ASCII→keycode 表。
