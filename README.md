# WallPicker

App leve em Rust (egui) para buscar papéis de parede em 2K/4K no [Wallhaven](https://wallhaven.cc) e aplicá-los no Windows.

## Uso

- Escolha um atalho ou digite um termo em inglês, e selecione 2K+ ou 4K+.
- Clique numa imagem para confirmar: **Aplicar**, **Só baixar** ou **Cancelar**.
- Botão direito baixa direto, sem confirmação.
- As imagens são salvas em `Imagens\Wallpapers`.

## Compilar

O projeto usa a toolchain `stable-x86_64-pc-windows-gnullvm` com o `rust-lld` embutido, então não precisa do Visual Studio Build Tools:

```bash
rustup toolchain install stable-x86_64-pc-windows-gnullvm
cargo build --release
```

O executável sai em `target/release/wallpicker.exe`.
