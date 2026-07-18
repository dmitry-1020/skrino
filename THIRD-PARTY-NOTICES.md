# Сторонние компоненты

Сам Skrino распространяется по проприетарной лицензии «просмотр только для
ознакомления» (см. [LICENSE](LICENSE)). Перечисленные ниже сторонние ресурсы и
библиотеки сохраняют свои собственные лицензии, и ограничения LICENSE на них
не распространяются.

## Встроенные шрифты

Бинарные файлы шрифтов лежат в `crates/skrino-app/assets/fonts/`.

- **Inter** (`Inter-Regular.otf`, `Inter-Medium.otf`, `Inter-SemiBold.otf`)
  Copyright The Inter Project Authors. Лицензия SIL Open Font License 1.1.
  https://github.com/rsms/inter
- **Phosphor Icons** (`Phosphor-PUA.ttf`, подмножество кодовых точек U+E000..U+F8FF)
  Copyright (c) Phosphor Icons. Лицензия MIT.
  https://github.com/phosphor-icons/core

## Библиотеки Rust

Полный список зависимостей и их версий зафиксирован в `Cargo.lock`. Все они
получены из публичного реестра crates.io и распространяются под собственными
лицензиями (преимущественно MIT и Apache-2.0). Ключевые из них: eframe/egui,
tiny-skia, image, xcap, windows-capture, suppaftp, russh и russh-sftp.
