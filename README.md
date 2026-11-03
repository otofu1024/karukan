# yuzukan IME

[Karukan](https://github.com/togatoga/karukan) をベースにした、ローカルLLMでかな漢字変換するmacOS向け日本語IMEです。

## 特徴

- **ローカル変換**: LLMと辞書を併用したライブ変換。モデルと辞書の取得後はオフラインで使えます。
- **macOSネイティブUI**: AppKit製のコンパクトな候補ウィンドウ。ライト・ダーク表示に対応します。
- **省メモリ化**: 辞書のメモリマップ化、同一トークナイザーの共有、CPU推論用の重みコピーの抑制。CPU推論に使わないMetal初期化も無効化しています。
- **描画・通信処理の改善**: 候補の行ビューを再利用し、通信処理の一時データを受信ごとに解放します。
- **辞書・学習・設定**: ユーザー辞書と変換履歴による学習に対応。ペルソナや、記号・空白の幅も設定できます。

Rustが変換処理、SwiftがmacOS連携とUI、llama.cppがCPUでモデル推論を担当します。

## 導入

macOS 13以降。Xcode Command Line Tools、Rust、CMakeが必要です。
Rust 1.99.1で動作確認しています。

```sh
git clone https://github.com/otofu1024/yuzukan.git
cd yuzukan/karukan-im/macos
make install
```

モデルと辞書は導入時に自動ダウンロードされます。
初回はログアウト・再ログインし、システム設定の「キーボード」→「入力ソース」からyuzukanを追加してください。

ソースからの手動ビルド・設定方法は [macOS版README](karukan-im/macos/README.md) を参照してください。

## ライセンス

コードは [MIT](LICENSE-MIT) または [Apache-2.0](LICENSE-APACHE)。
モデル・辞書データは各配布元のライセンスに従います。[第三者ライセンス](THIRD_PARTY_LICENSES)も参照してください。
