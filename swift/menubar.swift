import ApplicationServices
import AVFoundation
import Cocoa
import Carbon
import Sparkle

enum AgentState: String {
    case attention
    case working
    case idle
}

struct AgentInstance {
    let pid: Int
    let kind: String
    let project: String
    let branch: String
    let cwd: String
    let state: AgentState
    let ageSecs: Int?
}

struct AgentGroup {
    let kind: String
    let project: String
    let branch: String
    let state: AgentState
    let instances: [AgentInstance]

    var primaryPid: Int {
        instances[0].pid
    }

    var ageSecs: Int? {
        instances.compactMap(\.ageSecs).max()
    }
}

/// Manual override of the sleep behavior (mirrors the CLI's `asp force`).
enum SleepOverride: String {
    case auto
    case awake
    case sleep
}

struct InstanceList {
    let agents: [AgentInstance]
    let activeCount: Int
    let hooksInstalled: Bool
    let sleepDisabled: Bool
    let manualEnabled: Bool
    let force: SleepOverride
    let thermalWarning: Bool

    static let empty = InstanceList(
        agents: [], activeCount: 0, hooksInstalled: true, sleepDisabled: false,
        manualEnabled: true, force: .auto, thermalWarning: false)

    func withForce(_ value: SleepOverride) -> InstanceList {
        InstanceList(
            agents: agents, activeCount: activeCount, hooksInstalled: hooksInstalled,
            sleepDisabled: sleepDisabled, manualEnabled: manualEnabled,
            force: value, thermalWarning: thermalWarning)
    }

    func agents(in state: AgentState) -> [AgentInstance] {
        agents.filter { $0.state == state }
    }

    func groups(in state: AgentState) -> [AgentGroup] {
        var order: [String] = []
        var grouped: [String: [AgentInstance]] = [:]

        for agent in agents(in: state).sorted(by: {
            let left = ($0.project.lowercased(), $0.kind.lowercased(), $0.branch.lowercased(), $0.pid)
            let right = ($1.project.lowercased(), $1.kind.lowercased(), $1.branch.lowercased(), $1.pid)
            return left < right
        }) {
            let key = "\(agent.kind)\u{1f}\(agent.project)\u{1f}\(agent.branch)"
            if grouped[key] == nil {
                order.append(key)
            }
            grouped[key, default: []].append(agent)
        }

        return order.compactMap { key in
            guard let instances = grouped[key], let first = instances.first else {
                return nil
            }
            return AgentGroup(
                kind: first.kind,
                project: first.project,
                branch: first.branch,
                state: state,
                instances: instances
            )
        }
    }
}

private final class FlippedView: NSView {
    override var isFlipped: Bool { true }
}

private final class AgentRowView: NSControl {
    var onClick: (() -> Void)?
    private var trackingAreaRef: NSTrackingArea?

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        layer?.cornerRadius = 9
        layer?.backgroundColor = NSColor.labelColor.withAlphaComponent(0.045).cgColor
        setAccessibilityRole(.button)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    override var acceptsFirstResponder: Bool { true }

    override func hitTest(_ point: NSPoint) -> NSView? {
        bounds.contains(point) ? self : nil
    }

    override func mouseDown(with event: NSEvent) {
        layer?.backgroundColor = NSColor.selectedControlColor
            .withAlphaComponent(0.16).cgColor
    }

    override func mouseUp(with event: NSEvent) {
        updateBackground(hovered: bounds.contains(convert(event.locationInWindow, from: nil)))
        if bounds.contains(convert(event.locationInWindow, from: nil)) {
            activate()
        }
    }

    override func keyDown(with event: NSEvent) {
        if event.keyCode == 36 || event.keyCode == 49 {
            activate()
        } else {
            super.keyDown(with: event)
        }
    }

    override func accessibilityPerformPress() -> Bool {
        activate()
        return true
    }

    override func updateTrackingAreas() {
        if let trackingAreaRef {
            removeTrackingArea(trackingAreaRef)
        }
        let area = NSTrackingArea(
            rect: bounds,
            options: [.mouseEnteredAndExited, .activeAlways, .inVisibleRect],
            owner: self,
            userInfo: nil
        )
        addTrackingArea(area)
        trackingAreaRef = area
        super.updateTrackingAreas()
    }

    override func mouseEntered(with event: NSEvent) {
        updateBackground(hovered: true)
    }

    override func mouseExited(with event: NSEvent) {
        updateBackground(hovered: false)
    }

    private func updateBackground(hovered: Bool) {
        layer?.backgroundColor = hovered
            ? NSColor.selectedControlColor.withAlphaComponent(0.10).cgColor
            : NSColor.labelColor.withAlphaComponent(0.045).cgColor
    }

    private func activate() {
        onClick?()
    }
}

private final class AgentPopoverViewController: NSViewController {
    var onFocus: ((Int) -> Void)?
    var onSettings: (() -> Void)?
    var onInstallHooks: (() -> Void)?
    var onMore: ((NSButton) -> Void)?
    var onSleepOverride: ((SleepOverride) -> Void)?

    private var currentList = InstanceList.empty
    private var showAllIdle = false

    override func loadView() {
        let effect = NSVisualEffectView()
        effect.material = .popover
        effect.blendingMode = .behindWindow
        effect.state = .active
        view = effect
        render(.empty)
    }

    func render(_ list: InstanceList) {
        currentList = list
        guard isViewLoaded else { return }

        view.subviews.forEach { $0.removeFromSuperview() }

        let root = NSStackView()
        root.orientation = .vertical
        root.alignment = .leading
        root.spacing = 0
        root.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(root)

        let header = makeHeader(for: list)
        root.addArrangedSubview(header)
        header.widthAnchor.constraint(equalTo: root.widthAnchor).isActive = true

        let topRule = separator()
        root.addArrangedSubview(topRule)
        topRule.widthAnchor.constraint(equalTo: root.widthAnchor).isActive = true

        let scrollView = makeAgentList(for: list)
        root.addArrangedSubview(scrollView)
        scrollView.widthAnchor.constraint(equalTo: root.widthAnchor).isActive = true

        let bottomRule = separator()
        root.addArrangedSubview(bottomRule)
        bottomRule.widthAnchor.constraint(equalTo: root.widthAnchor).isActive = true

        let forceAwakeRow = makeSleepOverrideRow(for: list)
        root.addArrangedSubview(forceAwakeRow)
        forceAwakeRow.widthAnchor.constraint(equalTo: root.widthAnchor).isActive = true

        let footerRule = separator()
        root.addArrangedSubview(footerRule)
        footerRule.widthAnchor.constraint(equalTo: root.widthAnchor).isActive = true

        let footer = makeFooter(for: list)
        root.addArrangedSubview(footer)
        footer.widthAnchor.constraint(equalTo: root.widthAnchor).isActive = true

        NSLayoutConstraint.activate([
            root.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            root.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            root.topAnchor.constraint(equalTo: view.topAnchor),
            root.bottomAnchor.constraint(equalTo: view.bottomAnchor),
            header.heightAnchor.constraint(equalToConstant: 76),
            forceAwakeRow.heightAnchor.constraint(equalToConstant: 46),
            footer.heightAnchor.constraint(equalToConstant: 50),
            scrollView.heightAnchor.constraint(greaterThanOrEqualToConstant: 120),
        ])

        let rowCount = list.groups(in: .attention).count
            + list.groups(in: .working).count
            + min(list.groups(in: .idle).count, showAllIdle ? 8 : 3)
        let height = min(686, max(376, 226 + (rowCount * 54)))
        preferredContentSize = NSSize(width: 390, height: height)
    }

    private func makeHeader(for list: InstanceList) -> NSView {
        let container = NSView()
        let title = label("Agents", size: 17, weight: .semibold, color: .labelColor)
        let summary = label(summaryText(for: list), size: 12, color: .secondaryLabelColor)
        let textStack = NSStackView(views: [title, summary])
        textStack.orientation = .vertical
        textStack.alignment = .leading
        textStack.spacing = 3
        textStack.translatesAutoresizingMaskIntoConstraints = false

        let status = statusPill(for: list)
        status.translatesAutoresizingMaskIntoConstraints = false

        container.addSubview(textStack)
        container.addSubview(status)
        NSLayoutConstraint.activate([
            textStack.leadingAnchor.constraint(equalTo: container.leadingAnchor, constant: 18),
            textStack.centerYAnchor.constraint(equalTo: container.centerYAnchor),
            status.trailingAnchor.constraint(equalTo: container.trailingAnchor, constant: -18),
            status.centerYAnchor.constraint(equalTo: container.centerYAnchor),
        ])
        return container
    }

    private func makeAgentList(for list: InstanceList) -> NSScrollView {
        let scrollView = NSScrollView()
        scrollView.drawsBackground = false
        scrollView.hasVerticalScroller = true
        scrollView.autohidesScrollers = true
        scrollView.scrollerStyle = .overlay

        let document = FlippedView()
        document.translatesAutoresizingMaskIntoConstraints = false
        let stack = NSStackView()
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 14
        stack.edgeInsets = NSEdgeInsets(top: 14, left: 16, bottom: 14, right: 16)
        stack.translatesAutoresizingMaskIntoConstraints = false
        document.addSubview(stack)
        scrollView.documentView = document

        NSLayoutConstraint.activate([
            document.widthAnchor.constraint(equalTo: scrollView.contentView.widthAnchor),
            stack.leadingAnchor.constraint(equalTo: document.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: document.trailingAnchor),
            stack.topAnchor.constraint(equalTo: document.topAnchor),
            stack.bottomAnchor.constraint(equalTo: document.bottomAnchor),
        ])

        let attention = list.groups(in: .attention)
        let working = list.groups(in: .working)
        let idle = list.groups(in: .idle)

        if !attention.isEmpty {
            let section = makeSection(
                title: "NEEDS YOU",
                count: list.agents(in: .attention).count,
                color: .systemOrange,
                groups: attention
            )
            stack.addArrangedSubview(section)
            section.widthAnchor.constraint(equalTo: stack.widthAnchor, constant: -32).isActive = true
        }

        if !working.isEmpty {
            let section = makeSection(
                title: "WORKING",
                count: list.agents(in: .working).count,
                color: .systemGreen,
                groups: working
            )
            stack.addArrangedSubview(section)
            section.widthAnchor.constraint(equalTo: stack.widthAnchor, constant: -32).isActive = true
        }

        if attention.isEmpty && working.isEmpty && idle.isEmpty {
            let empty = makeEmptyState()
            stack.addArrangedSubview(empty)
            empty.widthAnchor.constraint(equalTo: stack.widthAnchor, constant: -32).isActive = true
        } else if !idle.isEmpty {
            let visibleIdle = showAllIdle ? idle : Array(idle.prefix(3))
            let section = makeSection(
                title: "IDLE",
                count: list.agents(in: .idle).count,
                color: .tertiaryLabelColor,
                groups: visibleIdle
            )
            stack.addArrangedSubview(section)
            section.widthAnchor.constraint(equalTo: stack.widthAnchor, constant: -32).isActive = true

            if idle.count > 3 {
                let hiddenCount = idle.count - 3
                let title = showAllIdle ? "Show less" : "Show \(hiddenCount) more idle projects"
                let showMore = NSButton(title: title, target: self, action: #selector(toggleIdle))
                showMore.isBordered = false
                showMore.font = NSFont.systemFont(ofSize: 11, weight: .medium)
                showMore.setAccessibilityLabel(title)
                stack.addArrangedSubview(showMore)
            }
        }

        return scrollView
    }

    private func makeSection(
        title: String,
        count: Int,
        color: NSColor,
        groups: [AgentGroup]
    ) -> NSView {
        let section = NSStackView()
        section.orientation = .vertical
        section.alignment = .leading
        section.spacing = 6

        let heading = label("\(title)  \(count)", size: 10, weight: .semibold, color: color)
        section.addArrangedSubview(heading)

        for group in groups {
            let row = makeAgentRow(group)
            section.addArrangedSubview(row)
            row.widthAnchor.constraint(equalTo: section.widthAnchor).isActive = true
            row.heightAnchor.constraint(equalToConstant: 48).isActive = true
        }
        return section
    }

    private func makeAgentRow(_ group: AgentGroup) -> AgentRowView {
        let row = AgentRowView()
        row.toolTip = group.instances.first?.cwd
        row.setAccessibilityLabel("\(group.project), \(group.kind), \(statusText(for: group))")
        row.onClick = { [weak self] in self?.onFocus?(group.primaryPid) }

        let color = stateColor(group.state)
        let badge = NSView()
        badge.wantsLayer = true
        badge.layer?.cornerRadius = 12
        badge.layer?.backgroundColor = color.withAlphaComponent(0.14).cgColor
        badge.translatesAutoresizingMaskIntoConstraints = false
        let initials = label(agentInitials(group.kind), size: 9, weight: .bold, color: color)
        initials.alignment = .center
        initials.translatesAutoresizingMaskIntoConstraints = false
        badge.addSubview(initials)

        let project = label(group.project, size: 13, weight: .semibold, color: .labelColor)
        project.lineBreakMode = .byTruncatingMiddle
        let metadata = label(metadataText(for: group), size: 11, color: .secondaryLabelColor)
        metadata.lineBreakMode = .byTruncatingTail
        let textStack = NSStackView(views: [project, metadata])
        textStack.orientation = .vertical
        textStack.alignment = .leading
        textStack.spacing = 2
        textStack.translatesAutoresizingMaskIntoConstraints = false

        let state = label(statusText(for: group), size: 11, weight: .medium, color: color)
        state.alignment = .right
        state.translatesAutoresizingMaskIntoConstraints = false
        state.setContentCompressionResistancePriority(.required, for: .horizontal)

        row.addSubview(badge)
        row.addSubview(textStack)
        row.addSubview(state)
        NSLayoutConstraint.activate([
            badge.leadingAnchor.constraint(equalTo: row.leadingAnchor, constant: 10),
            badge.centerYAnchor.constraint(equalTo: row.centerYAnchor),
            badge.widthAnchor.constraint(equalToConstant: 24),
            badge.heightAnchor.constraint(equalToConstant: 24),
            initials.leadingAnchor.constraint(equalTo: badge.leadingAnchor),
            initials.trailingAnchor.constraint(equalTo: badge.trailingAnchor),
            initials.centerYAnchor.constraint(equalTo: badge.centerYAnchor),
            textStack.leadingAnchor.constraint(equalTo: badge.trailingAnchor, constant: 10),
            textStack.centerYAnchor.constraint(equalTo: row.centerYAnchor),
            state.leadingAnchor.constraint(greaterThanOrEqualTo: textStack.trailingAnchor, constant: 8),
            state.trailingAnchor.constraint(equalTo: row.trailingAnchor, constant: -10),
            state.centerYAnchor.constraint(equalTo: row.centerYAnchor),
        ])
        return row
    }

    private func makeEmptyState() -> NSView {
        let container = NSView()
        let title = label("No agents running", size: 13, weight: .semibold, color: .labelColor)
        let detail = label(
            "Projects appear here when Claude Code, Codex, or Hermes starts.",
            size: 11,
            color: .secondaryLabelColor
        )
        detail.maximumNumberOfLines = 2
        detail.alignment = .center
        let stack = NSStackView(views: [title, detail])
        stack.orientation = .vertical
        stack.alignment = .centerX
        stack.spacing = 5
        stack.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(stack)
        NSLayoutConstraint.activate([
            container.heightAnchor.constraint(equalToConstant: 110),
            stack.centerXAnchor.constraint(equalTo: container.centerXAnchor),
            stack.centerYAnchor.constraint(equalTo: container.centerYAnchor),
            stack.widthAnchor.constraint(lessThanOrEqualTo: container.widthAnchor, constant: -30),
        ])
        return container
    }

    private func makeSleepOverrideRow(for list: InstanceList) -> NSView {
        let row = NSView()

        let control = NSSegmentedControl(
            labels: ["Force sleep", "Auto", "Force awake"],
            trackingMode: .selectOne,
            target: self,
            action: #selector(sleepOverrideChanged(_:))
        )
        control.controlSize = .small
        control.segmentDistribution = .fillEqually
        switch list.force {
        case .sleep: control.selectedSegment = 0
        case .auto: control.selectedSegment = 1
        case .awake: control.selectedSegment = 2
        }
        control.setAccessibilityLabel("Sleep override")
        control.translatesAutoresizingMaskIntoConstraints = false

        row.addSubview(control)
        NSLayoutConstraint.activate([
            control.leadingAnchor.constraint(equalTo: row.leadingAnchor, constant: 18),
            control.trailingAnchor.constraint(equalTo: row.trailingAnchor, constant: -18),
            control.centerYAnchor.constraint(equalTo: row.centerYAnchor),
        ])
        return row
    }

    private func makeFooter(for list: InstanceList) -> NSView {
        let footer = NSView()
        let settings = NSButton(title: "Settings", target: self, action: #selector(openSettings))
        settings.isBordered = false
        settings.font = NSFont.systemFont(ofSize: 12, weight: .medium)
        settings.translatesAutoresizingMaskIntoConstraints = false

        let more = NSButton(title: "•••", target: self, action: #selector(openMore(_:)))
        more.isBordered = false
        more.font = NSFont.systemFont(ofSize: 14, weight: .semibold)
        more.toolTip = "More actions"
        more.setAccessibilityLabel("More actions")
        more.translatesAutoresizingMaskIntoConstraints = false

        footer.addSubview(settings)
        footer.addSubview(more)

        var constraints = [
            settings.leadingAnchor.constraint(equalTo: footer.leadingAnchor, constant: 12),
            settings.centerYAnchor.constraint(equalTo: footer.centerYAnchor),
            more.trailingAnchor.constraint(equalTo: footer.trailingAnchor, constant: -12),
            more.centerYAnchor.constraint(equalTo: footer.centerYAnchor),
        ]

        if !list.hooksInstalled {
            let hooks = NSButton(
                title: "Install agent hooks",
                target: self,
                action: #selector(installHooks)
            )
            hooks.bezelStyle = .rounded
            hooks.controlSize = .small
            hooks.translatesAutoresizingMaskIntoConstraints = false
            footer.addSubview(hooks)
            constraints += [
                hooks.centerXAnchor.constraint(equalTo: footer.centerXAnchor),
                hooks.centerYAnchor.constraint(equalTo: footer.centerYAnchor),
            ]
        }

        NSLayoutConstraint.activate(constraints)
        return footer
    }

    private func statusPill(for list: InstanceList) -> NSView {
        let text: String
        let color: NSColor
        if list.thermalWarning {
            text = "THERMAL"
            color = .systemRed
        } else if list.force == .awake {
            text = "FORCED AWAKE"
            color = .systemIndigo
        } else if list.force == .sleep {
            text = "FORCED SLEEP"
            color = .systemOrange
        } else if list.sleepDisabled {
            text = "MAC AWAKE"
            color = .systemGreen
        } else if !list.manualEnabled {
            text = "PREVENTION OFF"
            color = .systemOrange
        } else {
            text = "READY"
            color = .secondaryLabelColor
        }

        let pill = NSView()
        pill.wantsLayer = true
        pill.layer?.cornerRadius = 10
        pill.layer?.backgroundColor = color.withAlphaComponent(0.13).cgColor
        let textLabel = label(text, size: 9, weight: .bold, color: color)
        textLabel.translatesAutoresizingMaskIntoConstraints = false
        pill.addSubview(textLabel)
        NSLayoutConstraint.activate([
            textLabel.leadingAnchor.constraint(equalTo: pill.leadingAnchor, constant: 9),
            textLabel.trailingAnchor.constraint(equalTo: pill.trailingAnchor, constant: -9),
            textLabel.topAnchor.constraint(equalTo: pill.topAnchor, constant: 5),
            textLabel.bottomAnchor.constraint(equalTo: pill.bottomAnchor, constant: -5),
        ])
        return pill
    }

    private func separator() -> NSBox {
        let rule = NSBox()
        rule.boxType = .separator
        return rule
    }

    private func label(
        _ text: String,
        size: CGFloat,
        weight: NSFont.Weight = .regular,
        color: NSColor
    ) -> NSTextField {
        let field = NSTextField(labelWithString: text)
        field.font = NSFont.systemFont(ofSize: size, weight: weight)
        field.textColor = color
        field.lineBreakMode = .byTruncatingTail
        field.maximumNumberOfLines = 1
        return field
    }

    private func summaryText(for list: InstanceList) -> String {
        let waiting = list.agents(in: .attention).count
        let working = list.agents(in: .working).count
        if waiting > 0 && working > 0 {
            return "\(waiting) need you  ·  \(working) working"
        }
        if waiting > 0 {
            return "\(waiting) need you"
        }
        if working > 0 {
            return "\(working) working"
        }
        return list.agents.isEmpty ? "No agents running" : "All agents idle"
    }

    private func metadataText(for group: AgentGroup) -> String {
        var parts = [group.kind]
        if !group.branch.isEmpty {
            parts.append(group.branch)
        }
        if group.instances.count > 1 {
            parts.append("\(group.instances.count) sessions")
        }
        return parts.joined(separator: "  ·  ")
    }

    private func statusText(for group: AgentGroup) -> String {
        if group.instances.count > 1 {
            switch group.state {
            case .attention: return "\(group.instances.count) need you"
            case .working: return "\(group.instances.count) working"
            case .idle: return "\(group.instances.count) idle"
            }
        }
        switch group.state {
        case .attention:
            return "Needs you"
        case .working:
            return group.ageSecs.map(shortDuration) ?? "Working"
        case .idle:
            return "Idle"
        }
    }

    private func shortDuration(_ seconds: Int) -> String {
        if seconds < 60 { return "\(seconds)s" }
        if seconds < 3600 { return "\(seconds / 60)m" }
        return "\(seconds / 3600)h"
    }

    private func stateColor(_ state: AgentState) -> NSColor {
        switch state {
        case .attention: return .systemOrange
        case .working: return .systemGreen
        case .idle: return .tertiaryLabelColor
        }
    }

    private func agentInitials(_ kind: String) -> String {
        if kind == "Claude Code" { return "CL" }
        if kind == "Codex" { return "CX" }
        if kind == "Hermes" { return "H" }
        return "AI"
    }

    @objc private func toggleIdle() {
        showAllIdle.toggle()
        render(currentList)
    }

    @objc private func openSettings() {
        onSettings?()
    }

    @objc private func sleepOverrideChanged(_ sender: NSSegmentedControl) {
        let mode: SleepOverride
        switch sender.selectedSegment {
        case 0: mode = .sleep
        case 2: mode = .awake
        default: mode = .auto
        }
        onSleepOverride?(mode)
    }

    @objc private func installHooks() {
        onInstallHooks?()
    }

    @objc private func openMore(_ sender: NSButton) {
        onMore?(sender)
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    private var statusItem: NSStatusItem?
    private var agentProcess: Process?
    private let popover = NSPopover()
    private let popoverController = AgentPopoverViewController()
    private var latestList = InstanceList.empty
    private var isRefreshing = false
    private var refreshPopoverOnNextList = false
    private var refreshRequestedWhileFetching = false
    private var pendingForce: SleepOverride?
    private let forceAwakeQueue = DispatchQueue(label: "asp.forceawake")
    private var permissionsWindow: NSWindow?
    private var permissionsTimer: Timer?
    private var lastAccessibilityGranted = false
    private var isInstalling = false
    private var isUninstalling = false
    private var statusRefreshTimer: Timer?
    private var hotKeyHandler: EventHandlerRef?
    private var hotKeyActive: EventHotKeyRef?
    private var hotKeyInactive: EventHotKeyRef?
    private let hotKeySignature: OSType = 0x61737021 // 'asp!'
    private let hotKeyQueue = DispatchQueue(label: "asp.hotkeys")
    private var activeIndex = 0
    private var inactiveIndex = 0
    private let statusRefreshInterval: TimeInterval = 2.0
    private let isPreview = ProcessInfo.processInfo.environment["ASP_UI_PREVIEW"] == "1"
    private lazy var updaterController = SPUStandardUpdaterController(
        startingUpdater: true,
        updaterDelegate: nil,
        userDriverDelegate: nil
    )

    func applicationDidFinishLaunching(_ notification: Notification) {
        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)

        let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        if let button = item.button {
            button.title = "Zz"
            button.target = self
            button.action = #selector(togglePopover(_:))
            button.sendAction(on: [.leftMouseUp])
            button.toolTip = "Agents Sleep Preventer"
        }
        statusItem = item

        popover.contentViewController = popoverController
        popover.behavior = isPreview ? .applicationDefined : .transient
        popover.animates = true
        popoverController.onFocus = { [weak self] pid in
            self?.popover.performClose(nil)
            self?.focusPid(pid)
        }
        popoverController.onSettings = { [weak self] in
            self?.popover.performClose(nil)
            self?.openSettings()
        }
        popoverController.onInstallHooks = { [weak self] in
            self?.popover.performClose(nil)
            self?.showInstallDialog()
        }
        popoverController.onMore = { [weak self] button in
            self?.showMoreMenu(from: button)
        }
        popoverController.onSleepOverride = { [weak self] mode in
            self?.setSleepOverride(mode)
        }

        if isPreview {
            apply(previewList())
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.25) {
                NSApp.activate(ignoringOtherApps: true)
                self.showPopover()
            }
            return
        }

        _ = updaterController
        startAgent()
        registerHotKeys()
        refreshMenu()
        startStatusRefreshTimer()
        promptInstallHooksIfNeeded()
        if !allPermissionsGranted() {
            showPermissionsPanel()
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        statusRefreshTimer?.invalidate()
        permissionsTimer?.invalidate()
        if !isPreview {
            unregisterHotKeys()
            stopAgent()
        }
    }

    @objc private func togglePopover(_ sender: NSStatusBarButton) {
        if popover.isShown {
            popover.performClose(sender)
            return
        }
        showPopover()
        refreshMenu()
    }

    private func showPopover() {
        guard let button = statusItem?.button else { return }
        _ = popoverController.view
        popoverController.render(latestList)
        popover.contentSize = popoverController.preferredContentSize
        popover.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
        popoverController.view.window?.makeFirstResponder(nil)
    }

    @objc private func quit() {
        NSApp.terminate(nil)
    }

    @objc private func openLogs() {
        let logDir = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Logs/AgentsSleepPreventer")
        NSWorkspace.shared.open(logDir)
    }

    @objc private func openSettings() {
        let cliPath = Bundle.main.bundleURL
            .appendingPathComponent("Contents/MacOS/asp")

        DispatchQueue.global(qos: .userInitiated).async {
            let process = Process()
            process.executableURL = cliPath
            process.arguments = ["settings"]
            process.standardOutput = FileHandle.nullDevice
            process.standardError = FileHandle.nullDevice
            do {
                try process.run()
                process.waitUntilExit()
            } catch {
                NSLog("Failed to open settings: \(error)")
            }
            DispatchQueue.main.async {
                self.refreshMenu()
            }
        }
    }

    private func setSleepOverride(_ mode: SleepOverride) {
        // Optimistic local update so intermediate renders keep the new state;
        // apply() overlays pendingForce onto snapshots fetched before the
        // CLI write lands.
        latestList = latestList.withForce(mode)
        pendingForce = mode
        updateStatusTitle(with: latestList)
        if isPreview {
            popoverController.render(latestList)
            pendingForce = nil
            return
        }

        let cliPath = Bundle.main.bundleURL
            .appendingPathComponent("Contents/MacOS/asp")
        // Serial queue: rapid clicks must reach the CLI in order, each
        // load-modify-save finishing before the next starts.
        forceAwakeQueue.async {
            let process = Process()
            process.executableURL = cliPath
            process.arguments = ["force", mode.rawValue]
            process.standardOutput = FileHandle.nullDevice
            process.standardError = FileHandle.nullDevice
            do {
                try process.run()
                process.waitUntilExit()
            } catch {
                NSLog("Failed to set force mode: \(error)")
            }
            DispatchQueue.main.async {
                // Only the newest click's completion clears the overlay
                if self.pendingForce == mode {
                    self.pendingForce = nil
                }
                self.refreshMenu()
            }
        }
    }

    private func startAgent() {
        guard agentProcess == nil else { return }
        let agentURL = Bundle.main.bundleURL
            .appendingPathComponent("Contents/MacOS/asp")

        let process = Process()
        process.executableURL = agentURL
        process.arguments = ["agent"]
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice

        do {
            try process.run()
            agentProcess = process
        } catch {
            NSLog("Failed to start asp agent: \(error)")
        }
    }

    private func stopAgent() {
        guard let process = agentProcess else { return }
        if process.isRunning {
            process.terminate()
            process.waitUntilExit()
        }
        agentProcess = nil
    }

    @objc private func installHooksAction() {
        showInstallDialog()
    }

    @objc private func uninstallAction() {
        showUninstallDialog()
    }

    private func refreshMenu(updatePopover: Bool = true) {
        refreshPopoverOnNextList = refreshPopoverOnNextList || updatePopover
        if isRefreshing {
            // Re-fetch once the in-flight fetch lands: its snapshot may
            // predate the write that motivated this request (e.g. the
            // force-awake toggle) and must not be the final word.
            refreshRequestedWhileFetching = true
            return
        }
        isRefreshing = true
        DispatchQueue.global(qos: .background).async {
            let list = self.fetchInstanceList()
            DispatchQueue.main.async {
                let shouldUpdatePopover = self.refreshPopoverOnNextList
                self.refreshPopoverOnNextList = false
                self.apply(list, updatePopover: shouldUpdatePopover)
                self.isRefreshing = false
                if self.refreshRequestedWhileFetching {
                    self.refreshRequestedWhileFetching = false
                    self.refreshMenu(updatePopover: shouldUpdatePopover)
                }
            }
        }
    }

    private func startStatusRefreshTimer() {
        statusRefreshTimer?.invalidate()
        let timer = Timer(
            timeInterval: statusRefreshInterval,
            repeats: true
        ) { [weak self] _ in
            self?.refreshMenu(updatePopover: false)
        }
        statusRefreshTimer = timer
        RunLoop.main.add(timer, forMode: .common)
    }

    private func apply(_ list: InstanceList, updatePopover: Bool = true) {
        var list = list
        if let pending = pendingForce {
            // A force write is in flight; this snapshot may predate it
            list = list.withForce(pending)
        }
        latestList = list
        updateStatusTitle(with: list)
        if updatePopover && popover.isShown {
            popoverController.render(list)
            popover.contentSize = popoverController.preferredContentSize
        }
    }

    private func fetchInstanceList() -> InstanceList {
        let agentURL = Bundle.main.bundleURL
            .appendingPathComponent("Contents/MacOS/asp")

        let process = Process()
        process.executableURL = agentURL
        process.arguments = ["list"]
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = FileHandle.nullDevice

        do {
            try process.run()
        } catch {
            return .empty
        }

        process.waitUntilExit()
        let hooksInstalled = isHooksInstalled()

        guard process.terminationStatus == 0 else {
            return InstanceList(
                agents: [], activeCount: 0, hooksInstalled: hooksInstalled, sleepDisabled: false,
                manualEnabled: true, force: .auto, thermalWarning: false)
        }

        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        guard
            let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else {
            return InstanceList(
                agents: [], activeCount: 0, hooksInstalled: hooksInstalled, sleepDisabled: false,
                manualEnabled: true, force: .auto, thermalWarning: false)
        }

        let agents = parseAgents(from: json)
        let activeCount = (json["active"] as? [Any])?.count
            ?? agents.filter { $0.state == .working }.count

        let sleepDisabled = (json["sleep_disabled"] as? NSNumber)?.boolValue ?? false
        let manualEnabled = (json["manual_enabled"] as? NSNumber)?.boolValue ?? true
        let force = (json["force"] as? String).flatMap(SleepOverride.init(rawValue:)) ?? .auto
        let thermalWarning = (json["thermal_warning"] as? NSNumber)?.boolValue ?? false

        return InstanceList(
            agents: agents, activeCount: activeCount, hooksInstalled: hooksInstalled,
            sleepDisabled: sleepDisabled, manualEnabled: manualEnabled,
            force: force, thermalWarning: thermalWarning)
    }

    private func parseAgents(from json: [String: Any]) -> [AgentInstance] {
        if let array = json["agents"] as? [[String: Any]] {
            return array.compactMap { item in
                guard
                    let pid = (item["pid"] as? NSNumber)?.intValue,
                    let stateText = item["state"] as? String,
                    let state = AgentState(rawValue: stateText)
                else {
                    return nil
                }

                let rawProject = (item["project"] as? String ?? "")
                    .trimmingCharacters(in: .whitespacesAndNewlines)
                let project = rawProject.isEmpty || rawProject == "unknown"
                    ? "Unknown project"
                    : rawProject
                return AgentInstance(
                    pid: pid,
                    kind: item["kind"] as? String ?? "Agent",
                    project: project,
                    branch: item["branch"] as? String ?? "",
                    cwd: item["cwd"] as? String ?? "",
                    state: state,
                    ageSecs: (item["age_secs"] as? NSNumber)?.intValue
                )
            }
        }

        // Compatibility with 4.2.0 while a newly built Swift app and CLI are
        // briefly mixed during local testing.
        var agents: [AgentInstance] = []
        var seen = Set<Int>()
        for item in json["active"] as? [[String: Any]] ?? [] {
            guard let pid = (item["pid"] as? NSNumber)?.intValue, seen.insert(pid).inserted else {
                continue
            }
            let identity = projectIdentity(from: item["location"] as? String ?? "")
            let attention = (item["attention"] as? NSNumber)?.boolValue ?? false
            agents.append(AgentInstance(
                pid: pid,
                kind: item["kind"] as? String ?? "Agent",
                project: identity.project,
                branch: identity.branch,
                cwd: "",
                state: attention ? .attention : .working,
                ageSecs: (item["age_secs"] as? NSNumber)?.intValue
            ))
        }
        for item in json["inactive"] as? [[String: Any]] ?? [] {
            guard let pid = (item["pid"] as? NSNumber)?.intValue, seen.insert(pid).inserted else {
                continue
            }
            let identity = projectIdentity(from: item["location"] as? String ?? "")
            let attention = (item["attention"] as? NSNumber)?.boolValue ?? false
            agents.append(AgentInstance(
                pid: pid,
                kind: item["kind"] as? String ?? "Agent",
                project: identity.project,
                branch: identity.branch,
                cwd: "",
                state: attention ? .attention : .idle,
                ageSecs: nil
            ))
        }
        for value in json["inactive"] as? [NSNumber] ?? [] {
            let pid = value.intValue
            guard seen.insert(pid).inserted else { continue }
            agents.append(AgentInstance(
                pid: pid,
                kind: "Agent",
                project: "Unknown project",
                branch: "",
                cwd: "",
                state: .idle,
                ageSecs: nil
            ))
        }
        return agents
    }

    private func projectIdentity(from location: String) -> (project: String, branch: String) {
        let marker = " git:("
        if let markerRange = location.range(of: marker), location.hasSuffix(")") {
            let project = String(location[..<markerRange.lowerBound])
            let branchStart = markerRange.upperBound
            let branch = String(location[branchStart..<location.index(before: location.endIndex)])
            return (project.isEmpty ? "Unknown project" : project, branch)
        }
        let project = location.isEmpty || location == "unknown" ? "Unknown project" : location
        return (project, "")
    }

    private func updateStatusTitle(with list: InstanceList) {
        guard let button = statusItem?.button else {
            return
        }
        button.image = nil
        // Under a thermal warning the CLI releases the sleep assertion even
        // when forced, so the title must not claim the Mac is kept awake.
        let forcedAwake = list.force == .awake && !list.thermalWarning
        if forcedAwake && list.activeCount == 0 {
            button.title = "ON"
            button.setAccessibilityLabel("Force keep awake enabled")
        } else if list.sleepDisabled || forcedAwake {
            let count = max(list.activeCount, 1)
            button.title = "ON \(count)"
            let noun = count == 1 ? "agent" : "agents"
            button.setAccessibilityLabel("\(count) \(noun) keeping the Mac awake")
        } else {
            button.title = "Zz"
            button.setAccessibilityLabel("No agents keeping the Mac awake")
        }
    }

    private func showMoreMenu(from button: NSButton) {
        let menu = NSMenu()
        let hooksTitle = latestList.hooksInstalled
            ? "Reinstall Agent Hooks..."
            : "Install Agent Hooks..."
        let hooks = NSMenuItem(
            title: hooksTitle,
            action: #selector(installHooksAction),
            keyEquivalent: "i"
        )
        hooks.target = self
        menu.addItem(hooks)

        let permissions = NSMenuItem(
            title: "Permissions...",
            action: #selector(showPermissionsPanelAction),
            keyEquivalent: ""
        )
        permissions.target = self
        menu.addItem(permissions)

        let updates = NSMenuItem(
            title: "Check for Updates...",
            action: #selector(checkForUpdates(_:)),
            keyEquivalent: ""
        )
        updates.target = self
        menu.addItem(updates)
        menu.addItem(.separator())

        let logs = NSMenuItem(title: "Open Logs", action: #selector(openLogs), keyEquivalent: "l")
        logs.target = self
        menu.addItem(logs)
        let uninstall = NSMenuItem(
            title: "Uninstall...",
            action: #selector(uninstallAction),
            keyEquivalent: ""
        )
        uninstall.target = self
        menu.addItem(uninstall)
        menu.addItem(.separator())

        let quitItem = NSMenuItem(title: "Quit", action: #selector(quit), keyEquivalent: "q")
        quitItem.target = self
        menu.addItem(quitItem)
        menu.popUp(positioning: nil, at: NSPoint(x: 0, y: button.bounds.maxY + 4), in: button)
    }

    @objc private func checkForUpdates(_ sender: Any?) {
        popover.performClose(nil)
        updaterController.checkForUpdates(sender)
    }

    private func previewList() -> InstanceList {
        func agent(
            _ pid: Int,
            _ kind: String,
            _ project: String,
            _ state: AgentState,
            branch: String = "main",
            age: Int? = nil
        ) -> AgentInstance {
            AgentInstance(
                pid: pid,
                kind: kind,
                project: project,
                branch: branch,
                cwd: "/Users/charles-andreassus/projects/\(project)",
                state: state,
                ageSecs: age
            )
        }

        return InstanceList(
            agents: [
                agent(12110, "Claude Code", "cleemo-lamdera", .attention),
                agent(16599, "Claude Code", "cleemo-lamdera", .attention),
                agent(14433, "Claude Code", "webhealth", .attention),
                agent(18206, "Claude Code", "capitole-immo-vision", .attention),
                agent(19729, "Claude Code", "thai-chess", .attention),
                agent(19959, "Claude Code", "archiscale", .attention),
                agent(25799, "Codex", "agents-sleep-preventer", .working, age: 34),
                agent(30101, "Hermes", "personal-branding", .idle),
                agent(30102, "Hermes", "declarimmo", .idle),
                agent(30103, "Hermes", "cleemo", .idle),
                agent(30104, "Hermes", "default", .idle, branch: ""),
                agent(30105, "Claude Code", "evo-hub", .idle),
            ],
            activeCount: 1,
            hooksInstalled: true,
            sleepDisabled: true,
            manualEnabled: true,
            force: .auto,
            thermalWarning: false
        )
    }

    private func registerHotKeys() {
        var eventType = EventTypeSpec(
            eventClass: OSType(kEventClassKeyboard),
            eventKind: UInt32(kEventHotKeyPressed)
        )

        let installStatus = InstallEventHandler(
            GetApplicationEventTarget(),
            { _, eventRef, userData in
                guard let userData = userData else {
                    return OSStatus(noErr)
                }
                let app = Unmanaged<AppDelegate>.fromOpaque(userData).takeUnretainedValue()
                var hotKeyId = EventHotKeyID()
                let status = GetEventParameter(
                    eventRef,
                    EventParamName(kEventParamDirectObject),
                    EventParamType(typeEventHotKeyID),
                    nil,
                    MemoryLayout<EventHotKeyID>.size,
                    nil,
                    &hotKeyId
                )
                if status == noErr && hotKeyId.signature == app.hotKeySignature {
                    app.handleHotKey(id: hotKeyId.id)
                }
                return OSStatus(noErr)
            },
            1,
            &eventType,
            UnsafeMutableRawPointer(Unmanaged.passUnretained(self).toOpaque()),
            &hotKeyHandler
        )

        if installStatus != noErr {
            NSLog("Failed to install hotkey handler: \(installStatus)")
            return
        }

        let modifiers = UInt32(controlKey | optionKey | cmdKey)
        let activeId = EventHotKeyID(signature: hotKeySignature, id: 1)
        let inactiveId = EventHotKeyID(signature: hotKeySignature, id: 2)

        let activeStatus = RegisterEventHotKey(
            UInt32(kVK_ANSI_J),
            modifiers,
            activeId,
            GetApplicationEventTarget(),
            0,
            &hotKeyActive
        )
        if activeStatus != noErr {
            NSLog("Failed to register active hotkey: \(activeStatus)")
        }

        let inactiveStatus = RegisterEventHotKey(
            UInt32(kVK_ANSI_L),
            modifiers,
            inactiveId,
            GetApplicationEventTarget(),
            0,
            &hotKeyInactive
        )
        if inactiveStatus != noErr {
            NSLog("Failed to register inactive hotkey: \(inactiveStatus)")
        }
    }

    private func unregisterHotKeys() {
        if let hotKeyActive = hotKeyActive {
            UnregisterEventHotKey(hotKeyActive)
        }
        if let hotKeyInactive = hotKeyInactive {
            UnregisterEventHotKey(hotKeyInactive)
        }
        if let hotKeyHandler = hotKeyHandler {
            RemoveEventHandler(hotKeyHandler)
        }
    }

    private func handleHotKey(id: UInt32) {
        if id == 1 {
            cycleActiveInstance()
        } else if id == 2 {
            cycleInactiveInstance()
        }
    }

    private func cycleActiveInstance() {
        hotKeyQueue.async {
            let list = self.fetchInstanceList()
            let agents = list.agents.filter { $0.state != .idle }
            guard !agents.isEmpty else {
                return
            }
            let idx = self.activeIndex % agents.count
            self.activeIndex = (self.activeIndex + 1) % agents.count
            self.focusPid(agents[idx].pid)
        }
    }

    private func cycleInactiveInstance() {
        hotKeyQueue.async {
            let list = self.fetchInstanceList()
            let agents = list.agents(in: .idle)
            guard !agents.isEmpty else {
                return
            }
            let idx = self.inactiveIndex % agents.count
            self.inactiveIndex = (self.inactiveIndex + 1) % agents.count
            self.focusPid(agents[idx].pid)
        }
    }

    private func focusPid(_ pid: Int) {
        if pid <= 0 {
            return
        }

        let agentURL = Bundle.main.bundleURL
            .appendingPathComponent("Contents/MacOS/asp")

        DispatchQueue.global(qos: .userInitiated).async {
            let process = Process()
            process.executableURL = agentURL
            process.arguments = ["focus", String(pid)]
            process.standardOutput = FileHandle.nullDevice
            process.standardError = FileHandle.nullDevice
            do {
                try process.run()
                process.waitUntilExit()
            } catch {
                NSLog("Failed to focus instance pid=\(pid): \(error)")
            }
        }
    }

    // MARK: - Permissions panel
    //
    // ASP needs exactly two TCC permissions, both for dictation:
    // Accessibility (hotkey event tap + text insertion) and Microphone.
    // The panel shows what's missing, deep-links to the right System
    // Settings pane, and re-checks live; once Accessibility appears the
    // agent is restarted so the hotkey listener actually starts.

    private func accessibilityGranted() -> Bool {
        AXIsProcessTrusted()
    }

    private func allPermissionsGranted() -> Bool {
        accessibilityGranted()
            && AVCaptureDevice.authorizationStatus(for: .audio) == .authorized
    }

    @objc private func showPermissionsPanelAction() {
        popover.performClose(nil)
        showPermissionsPanel()
    }

    private func showPermissionsPanel() {
        if permissionsWindow == nil {
            let window = NSWindow(
                contentRect: NSRect(x: 0, y: 0, width: 430, height: 190),
                styleMask: [.titled, .closable],
                backing: .buffered,
                defer: false
            )
            window.title = "ASP Permissions"
            window.isReleasedWhenClosed = false
            window.center()
            permissionsWindow = window
        }
        lastAccessibilityGranted = accessibilityGranted()
        renderPermissions()

        permissionsTimer?.invalidate()
        let timer = Timer(timeInterval: 1.0, repeats: true) { [weak self] _ in
            self?.permissionsTick()
        }
        RunLoop.main.add(timer, forMode: .common)
        permissionsTimer = timer

        NSApp.activate(ignoringOtherApps: true)
        permissionsWindow?.makeKeyAndOrderFront(nil)
    }

    private func permissionsTick() {
        guard let window = permissionsWindow, window.isVisible else {
            permissionsTimer?.invalidate()
            permissionsTimer = nil
            return
        }
        let granted = accessibilityGranted()
        if granted && !lastAccessibilityGranted && !isPreview {
            // The hotkey listener only starts at agent launch; pick the
            // fresh grant up immediately.
            stopAgent()
            startAgent()
        }
        lastAccessibilityGranted = granted
        renderPermissions()
    }

    private func renderPermissions() {
        guard let window = permissionsWindow else { return }

        let micStatus = AVCaptureDevice.authorizationStatus(for: .audio)
        let micRow: NSView
        switch micStatus {
        case .authorized:
            micRow = permissionRow(
                name: "Microphone",
                caption: "Voice recording for dictation",
                granted: true, actionTitle: "", action: nil)
        case .notDetermined:
            micRow = permissionRow(
                name: "Microphone",
                caption: "Voice recording for dictation",
                granted: false, actionTitle: "Allow…",
                action: #selector(requestMicAccess))
        default:
            micRow = permissionRow(
                name: "Microphone",
                caption: "Voice recording for dictation",
                granted: false, actionTitle: "Open Settings…",
                action: #selector(openMicSettings))
        }

        let stack = NSStackView(views: [
            permissionRow(
                name: "Accessibility",
                caption: "Dictation hotkey and text insertion",
                granted: accessibilityGranted(),
                actionTitle: "Open Settings…",
                action: #selector(openAccessibilitySettings)),
            micRow,
            {
                let note = label(
                    allPermissionsGranted()
                        ? "Everything is in place."
                        : "Statuses refresh automatically once granted.",
                    size: 11,
                    color: .secondaryLabelColor
                )
                return note
            }(),
        ])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 16
        stack.edgeInsets = NSEdgeInsets(top: 20, left: 20, bottom: 16, right: 20)
        stack.translatesAutoresizingMaskIntoConstraints = false

        let container = NSView()
        container.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: container.trailingAnchor),
            stack.topAnchor.constraint(equalTo: container.topAnchor),
        ])
        window.contentView = container
        for row in stack.arrangedSubviews where row is NSStackView {
            row.widthAnchor.constraint(equalTo: stack.widthAnchor, constant: -40).isActive = true
        }
    }

    private func permissionRow(
        name: String,
        caption: String,
        granted: Bool,
        actionTitle: String,
        action: Selector?
    ) -> NSView {
        let dot = label("●", size: 13, color: granted ? .systemGreen : .systemRed)

        let title = label(name, size: 13, weight: .semibold, color: .labelColor)
        let detail = label(caption, size: 11, color: .secondaryLabelColor)
        let text = NSStackView(views: [title, detail])
        text.orientation = .vertical
        text.alignment = .leading
        text.spacing = 2
        text.setContentHuggingPriority(.defaultLow, for: .horizontal)

        let row = NSStackView(views: [dot, text])
        row.orientation = .horizontal
        row.alignment = .centerY
        row.spacing = 10

        if granted {
            row.addArrangedSubview(label("Granted", size: 11, color: .systemGreen))
        } else if let action {
            let button = NSButton(title: actionTitle, target: self, action: action)
            button.bezelStyle = .rounded
            button.controlSize = .small
            row.addArrangedSubview(button)
        }
        return row
    }

    /// Reuses the popover's label style for the permissions panel.
    private func label(
        _ text: String,
        size: CGFloat,
        weight: NSFont.Weight = .regular,
        color: NSColor
    ) -> NSTextField {
        let field = NSTextField(labelWithString: text)
        field.font = NSFont.systemFont(ofSize: size, weight: weight)
        field.textColor = color
        field.lineBreakMode = .byTruncatingTail
        field.maximumNumberOfLines = 1
        return field
    }

    @objc private func openAccessibilitySettings() {
        // Prompts (once per app) and auto-adds ASP to the Accessibility list
        let options =
            [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary
        AXIsProcessTrustedWithOptions(options)
        if let url = URL(
            string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
        ) {
            NSWorkspace.shared.open(url)
        }
    }

    @objc private func requestMicAccess() {
        AVCaptureDevice.requestAccess(for: .audio) { _ in
            DispatchQueue.main.async {
                self.renderPermissions()
            }
        }
    }

    @objc private func openMicSettings() {
        if let url = URL(
            string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
        ) {
            NSWorkspace.shared.open(url)
        }
    }

    private func isHooksInstalled() -> Bool {
        let home = FileManager.default.homeDirectoryForCurrentUser
        let claudeHooksPath = home
            .appendingPathComponent(".claude/hooks/prevent-sleep.sh")
        let hasClaudeHooks = FileManager.default.fileExists(atPath: claudeHooksPath.path)
            && claudeSubagentHookInstalled(home)

        let codexHooksPath = home.appendingPathComponent(".codex/hooks.json")
        guard
            let hooksData = try? Data(contentsOf: codexHooksPath),
            let hooksText = String(data: hooksData, encoding: .utf8)
        else {
            return false
        }

        let hasCodexHooks = hooksText.contains("AgentsSleepPreventer.app/Contents/MacOS/asp")
            || hooksText.contains("/usr/local/bin/asp")
            || hooksText.contains("/usr/local/bin/agents-sleep-preventer")
        return hasClaudeHooks && hasCodexHooks
    }

    /// Mirrors the CLI's is_claude_hooks_installed: upgrades from
    /// pre-subagent versions must re-run setup to pick up the
    /// SubagentStart/SubagentStop/PostToolUse keep-awake hooks.
    private func claudeSubagentHookInstalled(_ home: URL) -> Bool {
        let settingsPath = home.appendingPathComponent(".claude/settings.json")
        guard let data = try? Data(contentsOf: settingsPath) else {
            return false // missing file: setup can create it
        }
        guard let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            // Unparseable settings.json: `asp install` would fail on the same
            // parse, so prompting setup could never succeed. Fail open like
            // the CLI and run with the existing script-based hooks.
            return true
        }
        guard
            let hooks = json["hooks"] as? [String: Any],
            let groups = hooks["SubagentStop"],
            let groupsData = try? JSONSerialization.data(withJSONObject: groups),
            let groupsText = String(data: groupsData, encoding: .utf8)
        else {
            return false
        }
        return groupsText.contains("refresh-sleep.sh")
    }

    private func promptInstallHooksIfNeeded() {
        if isHooksInstalled() {
            return
        }
        showInstallDialog(canCancel: false)
    }

    private func showInstallDialog(canCancel: Bool = true) {
        let alert = NSAlert()
        alert.messageText = "Install Agent Hooks"
        alert.informativeText = """
            This will configure Claude Code and Codex hooks to automatically prevent sleep while agents are working.

            The hooks will:
            • Prevent system sleep when an agent starts a task
            • Re-enable sleep when the agent finishes

            Administrator password required.
            """
        alert.addButton(withTitle: "Install Hooks")
        if canCancel {
            alert.addButton(withTitle: "Cancel")
        }

        let contentView = NSView(frame: NSRect(x: 0, y: 0, width: 300, height: 30))
        let debugCheck = NSButton(checkboxWithTitle: "Enable debug logging", target: nil, action: nil)
        debugCheck.state = .off
        debugCheck.frame = NSRect(x: 0, y: 5, width: 300, height: 20)
        contentView.addSubview(debugCheck)
        alert.accessoryView = contentView

        let response = alert.runModal()
        if response == .alertFirstButtonReturn {
            installHooks(debug: debugCheck.state == .on)
        }
    }

    private func installHooks(debug: Bool = false) {
        if isInstalling {
            return
        }
        isInstalling = true

        // TODO: Use debug flag when implemented in CLI
        _ = debug

        let cliPath = Bundle.main.bundleURL
            .appendingPathComponent("Contents/MacOS/asp")
            .path
        let realUser = NSUserName()
        let command = "SUDO_USER=\(realUser) \(cliPath) install -y"
        let applescript = "do shell script \"\(escapeForAppleScript(command))\" with administrator privileges"

        DispatchQueue.global(qos: .userInitiated).async {
            let process = Process()
            process.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
            process.arguments = ["-e", applescript]
            let errPipe = Pipe()
            process.standardError = errPipe

            do {
                try process.run()
            } catch {
                DispatchQueue.main.async {
                    self.isInstalling = false
                    self.showInstallError("Failed to start installer: \(error)")
                }
                return
            }

            process.waitUntilExit()
            let errData = errPipe.fileHandleForReading.readDataToEndOfFile()
            let errText = String(data: errData, encoding: .utf8) ?? ""
            let status = process.terminationStatus

            DispatchQueue.main.async {
                self.isInstalling = false
                if status == 0 {
                    self.showInstallSuccess()
                    self.refreshMenu()
                    return
                }
                if errText.contains("-128") || errText.contains("User canceled") {
                    return
                }
                self.showInstallError(errText.isEmpty ? "Install failed." : errText)
            }
        }
    }

    private func showInstallSuccess() {
        let alert = NSAlert()
        alert.messageText = "Setup complete"
        alert.informativeText = "Restart Claude Code or Codex to activate sleep prevention."
        alert.addButton(withTitle: "OK")
        alert.runModal()
    }

    private func showInstallError(_ message: String) {
        let alert = NSAlert()
        alert.messageText = "Setup failed"
        alert.informativeText = message
        alert.addButton(withTitle: "OK")
        alert.runModal()
    }

    private func escapeForAppleScript(_ text: String) -> String {
        return text
            .replacingOccurrences(of: "\\", with: "\\\\")
            .replacingOccurrences(of: "\"", with: "\\\"")
    }

    private func showUninstallDialog() {
        let alert = NSAlert()
        alert.messageText = "Uninstall Agents Sleep Preventer"
        alert.informativeText = "Select what to remove:"
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Uninstall")
        alert.addButton(withTitle: "Cancel")

        let contentView = NSView(frame: NSRect(x: 0, y: 0, width: 300, height: 100))

        let hooksCheck = NSButton(checkboxWithTitle: "Remove agent hooks", target: nil, action: nil)
        hooksCheck.state = .on
        hooksCheck.frame = NSRect(x: 0, y: 70, width: 300, height: 20)

        let modelCheck = NSButton(checkboxWithTitle: "Remove Whisper model (~1.5 GB)", target: nil, action: nil)
        modelCheck.state = .on
        modelCheck.frame = NSRect(x: 0, y: 45, width: 300, height: 20)

        let dataCheck = NSButton(checkboxWithTitle: "Remove app data and logs", target: nil, action: nil)
        dataCheck.state = .on
        dataCheck.frame = NSRect(x: 0, y: 20, width: 300, height: 20)

        contentView.addSubview(hooksCheck)
        contentView.addSubview(modelCheck)
        contentView.addSubview(dataCheck)
        alert.accessoryView = contentView

        let response = alert.runModal()
        if response == .alertFirstButtonReturn {
            performUninstall(
                removeHooks: hooksCheck.state == .on,
                removeModel: modelCheck.state == .on,
                removeData: dataCheck.state == .on
            )
        }
    }

    private func performUninstall(removeHooks: Bool, removeModel: Bool, removeData: Bool) {
        if isUninstalling {
            return
        }
        isUninstalling = true

        var args = ["uninstall"]
        if !removeModel {
            args.append("-k")
        }
        if !removeHooks {
            args.append("--keep-hooks")
        }
        if !removeData {
            args.append("--keep-data")
        }

        let cliPath = Bundle.main.bundleURL
            .appendingPathComponent("Contents/MacOS/asp")
            .path
        let realUser = NSUserName()
        let command = (["SUDO_USER=\(realUser)", cliPath] + args).joined(separator: " ")
        let applescript = "do shell script \"\(escapeForAppleScript(command))\" with administrator privileges"

        DispatchQueue.global(qos: .userInitiated).async {
            let process = Process()
            process.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
            process.arguments = ["-e", applescript]
            let errPipe = Pipe()
            process.standardError = errPipe

            do {
                try process.run()
            } catch {
                DispatchQueue.main.async {
                    self.isUninstalling = false
                    self.showUninstallError("Failed to start uninstaller: \(error)")
                }
                return
            }

            process.waitUntilExit()
            let errData = errPipe.fileHandleForReading.readDataToEndOfFile()
            let errText = String(data: errData, encoding: .utf8) ?? ""
            let status = process.terminationStatus

            DispatchQueue.main.async {
                self.isUninstalling = false
                if status == 0 {
                    self.showUninstallSuccess()
                    NSApp.terminate(nil)
                    return
                }
                if errText.contains("-128") || errText.contains("User canceled") {
                    return
                }
                self.showUninstallError(errText.isEmpty ? "Uninstall failed." : errText)
            }
        }
    }

    private func showUninstallSuccess() {
        let alert = NSAlert()
        alert.messageText = "Uninstall complete"
        alert.informativeText = "Agents Sleep Preventer has been removed."
        alert.addButton(withTitle: "OK")
        alert.runModal()
    }

    private func showUninstallError(_ message: String) {
        let alert = NSAlert()
        alert.messageText = "Uninstall failed"
        alert.informativeText = message
        alert.addButton(withTitle: "OK")
        alert.runModal()
    }
}

@main
struct ASPMenubarApp {
    static func main() {
        let app = NSApplication.shared
        let delegate = AppDelegate()
        app.delegate = delegate
        app.run()
    }
}
