import RunnerManagerKit
import SwiftUI

enum SidebarItem: Hashable {
    case overview
    case runner(Runner.ID)
}

struct ContentView: View {
    @Bindable var model: RunnerManagerModel
    @State var selection: SidebarItem? = .overview
    @State var isAddingRunner = false

    var body: some View {
        VStack(spacing: 0) {
            if model.isDemo { DemoBanner() }
            NavigationSplitView {
                Sidebar(model: model, selection: $selection, isAddingRunner: $isAddingRunner)
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
                .help("Reload the machine, the Nestor connection and the runners")
            }
            ToolbarItem {
                Button {
                    isAddingRunner = true
                } label: {
                    Label("Add Runner", systemImage: "plus")
                }
                .disabled(!model.isAssociated)
                .help(model.isAssociated ? "Add a runner on this machine" : "Associate this machine with Nestor first")
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
        .task { await model.load() }
        .onChange(of: model.runners) {
            // Leave a runner's page once it has been removed.
            if case .runner(let id) = selection, model.runners.value != nil, model.runner(id) == nil {
                selection = .overview
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
        case .overview, nil:
            OverviewView(model: model, isAddingRunner: $isAddingRunner)
        }
    }
}

struct Sidebar: View {
    var model: RunnerManagerModel
    @Binding var selection: SidebarItem?
    @Binding var isAddingRunner: Bool

    var body: some View {
        List(selection: $selection) {
            Label("This Mac", systemImage: "desktopcomputer")
                .tag(SidebarItem.overview)

            Section("Runners") {
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
                    Text("No runner yet").foregroundStyle(.secondary)
                case .loaded(let runners):
                    ForEach(runners) { runner in
                        HStack {
                            Label(runner.name, systemImage: "server.rack")
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
