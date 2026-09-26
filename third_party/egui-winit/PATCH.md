# Patched egui-winit 0.36.2

Unmodified copy of [egui-winit](https://github.com/emilk/egui) 0.36.2
(MIT OR Apache-2.0), except for `key_from_key_code` in `src/lib.rs`.

egui has no keys for the numeric keypad or the lock keys, and upstream maps
`Numpad1` to the same `Key::Num1` as the top-row `1`. A remote console must
send them as distinct USB usages, so this copy maps them onto F14–F35, which
`src/gui.rs` translates back:

| Physical key | egui key |
|---|---|
| Numpad0–Numpad9 | F25–F34 |
| NumpadDecimal / NumpadComma | F35 |
| NumpadAdd, Subtract, Multiply, Divide, Enter | F24, F23, F22, F21, F20 |
| NumLock, CapsLock, ScrollLock | F19, F18, F17 |
| PrintScreen, Pause, ContextMenu | F16, F15, F14 |

Real F14–F35 keys are therefore not available in the viewer.
