# Karukan IME（macOSカスタム版）

[Karukan](https://github.com/togatoga/karukan) をベースにした、ローカルLLMでかな漢字変換するmacOS向け日本語IMEです。

- 候補ウィンドウのデザインと描画処理を調整。
- 辞書のメモリマップ化、予測候補の省メモリ化、トークナイザー共有でメモリ使用量を削減。

Rustが変換処理、SwiftがmacOS連携とUI、llama.cppがCPUでモデル推論を担当します。

## 導入

macOS 13以降。HomebrewとXcode Command Line Toolsが必要です。

```sh
brew install --cask otofu1024/tap/karukan
```

Rust・CMakeを自動導入し、このMac向けにソースからビルドします。
モデルと辞書も導入時に自動ダウンロードされます。
初回はログアウト・再ログインし、システム設定の「キーボード」→「入力ソース」からKarukanを追加してください。

ソースからの手動ビルド・設定方法は [macOS版README](karukan-im/macos/README.md) を参照してください。

## ライセンス

コードは [MIT](LICENSE-MIT) または [Apache-2.0](LICENSE-APACHE)。
モデル・辞書データは各配布元のライセンスに従います。[第三者ライセンス](THIRD_PARTY_LICENSES)も参照してください。
