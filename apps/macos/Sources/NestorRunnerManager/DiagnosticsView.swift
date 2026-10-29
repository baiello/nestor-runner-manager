import RunnerManagerKit
import SwiftUI

/// The runners, for diagnosis. The service starts and stops them to match the tasks allowed on
/// the machine; nothing here is needed in normal use.
struct DiagnosticsView: View {
    var model: RunnerManagerModel
    @Binding var selection: SidebarItem?
    @Binding var isAddingRunner: Bool

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                Text("Runners").font(.largeTitle.bold())
                Text("The service starts a runner for each task it accepts and stops idle ones. A runner takes one task at a time; that task may use several threads and processes. Use this page to diagnose a problem.")
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                runnersCard
                advancedCard
            }
            .padding(24)
            .frame(maxWidth: 760, alignment: .leading)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .navigationTitle("Runners")
    }

    private var runnersCard: some View {
        Card(title: "Runners on this machine", systemImage: "server.rack") {
            switch model.runners {
            case .loading:
                LoadingRow(text: "Loading the runners…")
            case .failed(let message):
                ErrorRow(message: message) { Task { await model.load() } }
            case .loaded(let runners) where runners.isEmpty:
                Text("No runner right now. The service starts one when there is a task to take.")
                    .foregroundStyle(.secondary)
            case .loaded(let runners):
                let counts = Dictionary(grouping: runners, by: \.state).mapValues(\.count)
                HStack(spacing: 20) {
                    ForEach(RunnerState.allCases, id: \.self) { state in
                        if let count = counts[state] {
                            HStack(spacing: 6) {
                                StateDot(state: state)
                                Text("\(count) \(state.title.lowercased())")
                            }
                        }
                    }
                }
                Divider()
                VStack(alignment: .leading, spacing: 8) {
                    ForEach(runners) { runner in
                        HStack(spacing: 10) {
                            Button(runner.name) { selection = .runner(runner.id) }
                                .buttonStyle(.link)
                            StateBadge(state: runner.state)
                            Spacer()
                            if let task = runner.currentTaskRef {
                                Text(task).font(.callout.monospaced()).foregroundStyle(.secondary)
                            }
                        }
                    }
                }
            }
        }
    }

    private var advancedCard: some View {
        Card(title: "Advanced", systemImage: "wrench.and.screwdriver") {
            Text("Add a runner by hand only to diagnose a problem. Once started, it takes tasks like the others, within the number of tasks the machine accepts.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            Button("Add a Runner by Hand…") { isAddingRunner = true }
                .disabled(!model.isAssociated)
                .help(model.isAssociated ? "Add a runner on this machine" : "Associate this machine with Nestor first")
        }
    }
}
