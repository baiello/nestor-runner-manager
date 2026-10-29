import RunnerManagerKit
import SwiftUI

enum SidebarItem: Hashable {
    case overview
    /// The runners, as a diagnostic: the service manages them.
    case diagnostics
    case runner(Runner.ID)
}

struct ContentView: View {
    @Bindable var model: RunnerManagerModel
    @State var selection: SidebarItem? = .overview
    @State var isAddingRunner = false

    var body: some View {
        VStack(spacing: 0) {
            if model.isDemo { DemoBanner() }
            if let access = model.access, let banner = AccessBanner(access: access) { banner }
            NavigationSplitView {
                Sidebar(model: model, selection: $selection)
                    .navigationSplitViewColumnWidth(min: 220, ideal: 240)
            } detail: {
                detail
            }
        }
        .toolbar {
            ToolbarItem {
                Button {
                    Task { await model.load() }
                } label: {
                    Label("Refresh", systemImage: "arrow.clockwise")
                }
                .help("Reload the machine, its tasks and the Nestor connection")
            }
        }
        .sheet(isPresented: $isAddingRunner) {
            AddRunnerSheet(model: model) { runner in
                selection = .runner(runner.id)
            }
        }
        .alert(
            "The command failed",
            isPresented: Binding(get: { model.actionError != nil }, set: { if !$0 { model.actionError = nil } }),
            presenting: model.actionError
        ) { _ in
            Button("OK", role: .cancel) {}
        } message: { message in
            Text(message)
        }
        .task {
            await model.load()
            // Tasks start and end on their own: keep the status current.
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(3))
                await model.refresh()
            }
        }
        .onChange(of: model.runners) {
            // Leave a runner's page once it has been removed.
            if case .runner(let id) = selection, model.runners.value != nil, model.runner(id) == nil {
                selection = .diagnostics
            }
        }
    }

    @ViewBuilder private var detail: some View {
        switch selection {
        case .runner(let id):
            if let runner = model.runner(id) {
                RunnerDetailView(model: model, runner: runner)
            } else {
                ContentUnavailableView("No runner selected", systemImage: "server.rack")
            }
        case .diagnostics:
            DiagnosticsView(model: model, selection: $selection, isAddingRunner: $isAddingRunner)
        case .overview, nil:
            OverviewView(model: model, selection: $selection)
        }
    }
}

struct Sidebar: View {
    var model: RunnerManagerModel
    @Binding var selection: SidebarItem?

    var body: some View {
        List(selection: $selection) {
            Label("This Mac", systemImage: "desktopcomputer")
                .tag(SidebarItem.overview)

            Section("Diagnostics") {
                Label("Runners", systemImage: "stethoscope")
                    .tag(SidebarItem.diagnostics)
                switch model.runners {
                case .loading:
                    HStack(spacing: 8) {
                        ProgressView().controlSize(.small)
                        Text("Loading…").foregroundStyle(.secondary)
                    }
                case .failed:
                    Label("Unavailable", systemImage: "exclamationmark.triangle")
                        .foregroundStyle(.secondary)
                case .loaded(let runners) where runners.isEmpty:
                    EmptyView()
                case .loaded(let runners):
                    ForEach(runners) { runner in
                        HStack {
                            Label(runner.name, systemImage: "server.rack")
                                .padding(.leading, 8)
                            Spacer()
                            StateDot(state: runner.state)
                        }
                        .tag(SidebarItem.runner(runner.id))
                        .accessibilityElement(children: .combine)
                        .accessibilityValue(runner.state.title)
                    }
                }
            }
        }
        .listStyle(.sidebar)
    }
}

/// Shown on every screen while the service is simulated, so no simulated command passes for a real one.
/// Shown when the service does not give this app full access, or gives it only because it was
/// started for development with unsigned clients allowed.
struct AccessBanner: View {
    let text: String
    let isWarning: Bool

    init?(access: ServiceAccess) {
        switch (access.level, access.verification) {
        case (.full, .unsignedFallback):
            text = "Development: the runner service accepts unsigned apps (--allow-unsigned-clients). Do not use it for real work."
            isWarning = true
        case (.full, _):
            return nil
        case (.readOnly, _), (.basic, _):
            text = "Read only: the runner service could not verify this app's signature, so it shows the status but refuses changes."
            isWarning = false
        }
    }

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: isWarning ? "exclamationmark.triangle.fill" : "lock.fill")
            Text(text).lineLimit(2)
            Spacer(minLength: 0)
        }
        .font(.callout)
        .foregroundStyle(isWarning ? Color.black : Color.primary)
        .padding(.horizontal, 14)
        .padding(.vertical, 7)
        .frame(maxWidth: .infinity)
        .background(isWarning ? Color.orange : Color.secondary.opacity(0.15))
        .accessibilityElement(children: .combine)
    }
}

struct DemoBanner: View {
    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "theatermasks.fill")
            Text("Demo mode").bold()
            Text("— simulated data. Nothing is sent to Nestor, no runner is installed and no secret is stored.")
                .lineLimit(2)
            Spacer(minLength: 0)
        }
        .font(.callout)
        .foregroundStyle(.black)
        .padding(.horizontal, 14)
        .padding(.vertical, 7)
        .frame(maxWidth: .infinity)
        .background(Color.yellow)
        .accessibilityElement(children: .combine)
        .accessibilityLabel("Demo mode: simulated data. Nothing is sent to Nestor, no runner is installed and no secret is stored.")
    }
}
