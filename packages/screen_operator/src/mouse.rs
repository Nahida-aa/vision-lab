//! 鼠标按键定义与 ydotool 键码编码。
//!
//! ydotool 的 `click` 子命令用「按下位 0x40 + 抬起位 0x80 + 按键索引」组合成键码：
//! 完整左键点击 = `0x40|0x00|0x80` = `0xC0`。按下/抬起分离则只用对应位。

/// 鼠标按键。编码与 ydotool 的键码索引一致（0=左 1=右 2=中 …）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Side,
    Extra,
    Forward,
    Back,
    Task,
}

impl MouseButton {
    /// ydotool 的按键索引位（0x00 左、0x01 右、0x02 中 …）。
    fn index(&self) -> u8 {
        match self {
            MouseButton::Left => 0x00,
            MouseButton::Right => 0x01,
            MouseButton::Middle => 0x02,
            MouseButton::Side => 0x03,
            MouseButton::Extra => 0x04,
            MouseButton::Forward => 0x05,
            MouseButton::Back => 0x06,
            MouseButton::Task => 0x07,
        }
    }
    /// 仅「按下」的键码（down 位 0x40 | 索引）。
    pub fn down_code(&self) -> u8 {
        0x40 | self.index()
    }
    /// 仅「抬起」的键码（up 位 0x80 | 索引）。
    pub fn up_code(&self) -> u8 {
        0x80 | self.index()
    }
    /// 「按下并抬起」的完整点击键码（down|up）。
    pub fn click_code(&self) -> u8 {
        self.down_code() | self.up_code()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_codes() {
        assert_eq!(MouseButton::Left.click_code(), 0xC0);
        assert_eq!(MouseButton::Right.down_code(), 0x41);
        assert_eq!(MouseButton::Right.up_code(), 0x81);
        assert_eq!(MouseButton::Middle.click_code(), 0xC2);
    }
}
