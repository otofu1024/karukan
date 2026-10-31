import Cocoa

/// Custom candidate window (borderless non-activating NSPanel).
///
/// The engine pre-paginates: `show` receives only the visible page plus
/// page metadata, so this controller just renders rows. An optional aux
/// line (reading hint / model info from the engine) is shown as a footer.
class CandidateWindowController {
    private static let candidateFontSize: CGFloat = 18
    private static let footerFontSize: CGFloat = 13
    private static let numberFontSize: CGFloat = 13
    private static let minPanelWidth: CGFloat = 160
    private static let backgroundColor = NSColor(name: nil) { appearance in
        appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
            ? NSColor(srgbRed: 0.07, green: 0.08, blue: 0.09, alpha: 0.98)
            : NSColor(calibratedWhite: 0.985, alpha: 0.98)
    }
    private static let selectionColor = NSColor(name: nil) { appearance in
        appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
            ? NSColor(srgbRed: 0.14, green: 0.17, blue: 0.23, alpha: 1)
            : NSColor(srgbRed: 0.90, green: 0.93, blue: 0.97, alpha: 1)
    }
    private static let borderColor = NSColor(name: nil) { appearance in
        appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
            ? NSColor(srgbRed: 0.24, green: 0.27, blue: 0.30, alpha: 1)
            : NSColor(srgbRed: 0.80, green: 0.82, blue: 0.85, alpha: 1)
    }
    private static let sourceSymbols = [
        ("📝", "学習", "clock.arrow.circlepath"),
        ("👤", "ユーザー", "person.crop.circle"),
        ("🤖", "AI", "cpu"),
        ("📚", "辞書", "book.closed"),
        ("🔄", "変換", "arrow.triangle.2.circlepath"),
        ("📅", "日付", "calendar"),
    ]

    private let panel: NSPanel
    private let stackView: NSStackView
    private var rowViews: [NSView] = []
    private var auxText: String?

    private struct PageState {
        let candidates: [CandidateItem]
        let cursor: Int
        let page: Int
        let totalPages: Int
    }
    private var pageState: PageState?

    init() {
        panel = NSPanel(
            contentRect: NSRect(x: 0, y: 0, width: 200, height: 100),
            styleMask: [.nonactivatingPanel, .borderless],
            backing: .buffered,
            defer: true
        )
        panel.level = .popUpMenu
        panel.hidesOnDeactivate = false
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = true
        panel.ignoresMouseEvents = true

        let background = NSBox(frame: panel.contentView?.bounds ?? .zero)
        background.boxType = .custom
        background.titlePosition = .noTitle
        background.contentViewMargins = .zero
        background.cornerRadius = 14
        background.borderWidth = 1
        background.borderColor = Self.borderColor
        background.fillColor = Self.backgroundColor
        panel.contentView = background

        stackView = NSStackView()
        stackView.orientation = .vertical
        stackView.alignment = .leading
        stackView.distribution = .fill
        stackView.spacing = 3
        stackView.edgeInsets = NSEdgeInsets(top: 8, left: 8, bottom: 8, right: 8)
        stackView.translatesAutoresizingMaskIntoConstraints = false

        if let contentView = background.contentView {
            contentView.addSubview(stackView)
            NSLayoutConstraint.activate([
                stackView.topAnchor.constraint(equalTo: contentView.topAnchor),
                stackView.leadingAnchor.constraint(equalTo: contentView.leadingAnchor),
                stackView.trailingAnchor.constraint(equalTo: contentView.trailingAnchor),
                stackView.bottomAnchor.constraint(equalTo: contentView.bottomAnchor),
            ])
        }
    }

    var isVisible: Bool { panel.isVisible }

    /// `cursorRect: nil` reuses the rect from the previous `show` — the
    /// caller can skip its (synchronous, per-keystroke) client IPC while
    /// the panel is already on screen, since the composition anchor
    /// doesn't move mid-composition.
    func show(
        candidates: [CandidateItem], cursor: Int, page: Int, totalPages: Int, cursorRect: NSRect?
    ) {
        pageState = PageState(
            candidates: candidates, cursor: cursor, page: page, totalPages: totalPages)
        render(cursorRect: cursorRect)
    }

    /// Update the aux footer; re-renders in place if the window is visible.
    /// Pass `deferRender: true` when a `show`/`hide` follows in the same
    /// action batch, so the panel is rendered once per batch instead of
    /// once for the aux change and again for the candidates.
    func setAux(_ text: String?, deferRender: Bool = false) {
        auxText = text
        if !deferRender, panel.isVisible, pageState != nil {
            render(cursorRect: nil)
        }
    }

    func hide() {
        pageState = nil
        panel.orderOut(nil)
    }

    private func render(cursorRect: NSRect?) {
        clearRows()
        guard let state = pageState else {
            hide()
            return
        }
        // An empty list still renders while the aux footer has text: a
        // source-filtered view narrowed to an empty source must show its
        // 「候補なし」 footer, not a silently vanishing panel.
        if state.candidates.isEmpty && (auxText ?? "").isEmpty {
            hide()
            return
        }

        for (index, candidate) in state.candidates.enumerated() {
            addCandidateRow(candidate, number: index + 1, selected: index == state.cursor)
        }
        var footer = auxText ?? ""
        if state.totalPages > 1 {
            let page = "(\(state.page + 1)/\(state.totalPages))"
            if !footer.contains(page) {
                footer = footer.isEmpty ? page : "\(footer) \(page)"
            }
        }
        if !footer.isEmpty {
            addFooterSeparator()
            addFooterLabel(footer)
        }

        positionPanel(cursorRect: cursorRect)
    }

    private func clearRows() {
        for view in rowViews {
            stackView.removeArrangedSubview(view)
            view.removeFromSuperview()
        }
        rowViews.removeAll()
    }

    private func addCandidateRow(_ candidate: CandidateItem, number: Int, selected: Bool) {
        let numberLabel = NSTextField(labelWithString: "\(number)")
        numberLabel.font = NSFont.systemFont(ofSize: Self.numberFontSize)
        numberLabel.alignment = .center
        numberLabel.textColor = selected ? .labelColor : .secondaryLabelColor
        numberLabel.translatesAutoresizingMaskIntoConstraints = false

        let numberColumn = NSView()
        numberColumn.translatesAutoresizingMaskIntoConstraints = false
        numberColumn.addSubview(numberLabel)
        NSLayoutConstraint.activate([
            numberColumn.widthAnchor.constraint(equalToConstant: 24),
            numberColumn.heightAnchor.constraint(equalToConstant: 21),
            numberLabel.centerXAnchor.constraint(equalTo: numberColumn.centerXAnchor),
            numberLabel.centerYAnchor.constraint(equalTo: numberColumn.centerYAnchor),
        ])

        let text = NSMutableAttributedString(
            string: candidate.text,
            attributes: [
                .font: NSFont.systemFont(ofSize: Self.candidateFontSize),
                .foregroundColor: NSColor.labelColor,
            ]
        )
        if let description = candidate.description {
            text.append(
                NSAttributedString(
                    string: "  \(description)",
                    attributes: [
                        .font: NSFont.systemFont(ofSize: Self.footerFontSize),
                        .foregroundColor: NSColor.secondaryLabelColor,
                    ]
                ))
        }

        let candidateLabel = NSTextField(labelWithAttributedString: text)
        candidateLabel.translatesAutoresizingMaskIntoConstraints = false
        candidateLabel.alignment = .left
        candidateLabel.setContentCompressionResistancePriority(.required, for: .horizontal)

        let row = NSStackView(views: [numberColumn, candidateLabel])
        row.translatesAutoresizingMaskIntoConstraints = false
        row.orientation = .horizontal
        row.alignment = .centerY
        row.distribution = .fill
        row.spacing = 10
        row.edgeInsets = NSEdgeInsets(top: 3, left: 10, bottom: 3, right: 10)
        row.wantsLayer = true
        row.layer?.cornerRadius = 8
        row.layer?.masksToBounds = true
        panel.effectiveAppearance.performAsCurrentDrawingAppearance {
            row.layer?.backgroundColor = selected ? Self.selectionColor.cgColor : NSColor.clear.cgColor
        }
        stackView.addArrangedSubview(row)
        row.widthAnchor.constraint(equalTo: stackView.widthAnchor, constant: -16).isActive = true
        rowViews.append(row)
    }

    private func addFooterSeparator() {
        let separator = NSBox()
        separator.boxType = .separator
        separator.translatesAutoresizingMaskIntoConstraints = false
        separator.heightAnchor.constraint(equalToConstant: 1).isActive = true
        stackView.addArrangedSubview(separator)
        separator.widthAnchor.constraint(equalTo: stackView.widthAnchor, constant: -16).isActive = true
        rowViews.append(separator)
    }

    private func addFooterLabel(_ text: String) {
        let label = NSTextField(labelWithAttributedString: Self.styledFooter(text))
        label.translatesAutoresizingMaskIntoConstraints = false
        label.setContentCompressionResistancePriority(.required, for: .horizontal)
        stackView.addArrangedSubview(label)
        label.widthAnchor.constraint(equalTo: stackView.widthAnchor, constant: -16).isActive = true
        rowViews.append(label)
    }

    private static func styledFooter(_ text: String) -> NSAttributedString {
        var text = text
        if text.hasPrefix("⚡") {
            text.removeFirst()
            text = text.trimmingCharacters(in: .whitespaces)
        }
        let source = text as NSString
        var symbols: [(NSRange, String, String)] = []
        // Match engine metadata tokens, preserving emoji in the user's reading and context.
        for (emoji, label, symbol) in sourceSymbols {
            for token in ["[変換:\(emoji)]", "| \(emoji) \(label)"] {
                let tokenRange = source.range(of: token, options: .backwards)
                if tokenRange.location != NSNotFound {
                    symbols.append((source.range(of: emoji, range: tokenRange), symbol, label))
                }
            }
        }
        if text.hasPrefix("[☺]") {
            symbols.append((source.range(of: "☺"), "face.smiling", "絵文字入力"))
        }
        if text.contains("[変換"), text.contains(" | 推論:") {
            let range = source.range(of: " 🎯 ")
            if range.location != NSNotFound {
                symbols.append((source.range(of: "🎯", range: range), "scope", "変換範囲"))
            }
        }
        let font = NSFont.systemFont(ofSize: footerFontSize)
        let result = NSMutableAttributedString(
            string: text, attributes: [.font: font, .foregroundColor: NSColor.secondaryLabelColor])
        let configuration = NSImage.SymbolConfiguration(pointSize: footerFontSize, weight: .regular)
            .applying(NSImage.SymbolConfiguration(paletteColors: [.secondaryLabelColor]))
        for (range, symbol, description) in symbols.sorted(by: { $0.0.location > $1.0.location }) {
            let attachment = NSTextAttachment()
            attachment.image = NSImage(systemSymbolName: symbol, accessibilityDescription: description)?
                .withSymbolConfiguration(configuration)
            attachment.bounds = NSRect(x: 0, y: -2, width: footerFontSize, height: footerFontSize)
            result.replaceCharacters(in: range, with: NSAttributedString(attachment: attachment))
        }
        return result
    }

    private var lastCursorRect: NSRect = .zero

    private func positionPanel(cursorRect: NSRect?) {
        if let rect = cursorRect {
            lastCursorRect = rect
        }
        let cursorRect = lastCursorRect

        stackView.layoutSubtreeIfNeeded()
        let contentSize = stackView.fittingSize
        let panelWidth = max(ceil(contentSize.width) + 2, Self.minPanelWidth)
        let panelHeight = ceil(contentSize.height) + 2

        guard cursorRect != .zero else {
            panel.setFrame(
                NSRect(x: 100, y: 100, width: panelWidth, height: panelHeight), display: true)
            panel.orderFront(nil)
            return
        }

        // Flip above the cursor when the panel would fall off the bottom of
        // the screen.
        let showAbove: Bool
        if let screen = NSScreen.main {
            showAbove = cursorRect.origin.y - panelHeight < screen.visibleFrame.origin.y
        } else {
            showAbove = false
        }

        let originY: CGFloat
        if showAbove {
            originY = cursorRect.origin.y + cursorRect.size.height
        } else {
            originY = cursorRect.origin.y - panelHeight
        }

        panel.setFrame(
            NSRect(x: cursorRect.origin.x, y: originY, width: panelWidth, height: panelHeight),
            display: true)
        panel.orderFront(nil)
    }
}
