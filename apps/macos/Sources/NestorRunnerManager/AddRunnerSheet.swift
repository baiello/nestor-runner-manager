import RunnerManagerKit
import SwiftUI

/// The add-runner flow. It asks for no secret: Nestor issues the runner's credential.
struct AddRunnerSheet: View {
    var model: RunnerManagerModel
    var onAdded: (Runner) -> Void
    @Environment(\.dismiss) private var dismiss

    @State var name = ""
    @State var limitsCPU = false
    @State var cpuCores = 4
    @State var limitsMemory = false
    @State var memoryGiB = 16
    @State var concurrentJobs = 1
    @State var startNow = true
    @State var isSubmitting = false
    @State var error: String?

    private var nameError: String? {
        guard !name.isEmpty else { return nil }
        do {
            _ = try RunnerNameRule.validate(name)
            return nil
        } catch {
            return (error as? LocalizedError)?.errorDescription
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("Add a Runner").font(.title2.bold()).padding([.top, .horizontal], 20)
            Form {
                Section {
                    TextField("Name", text: $name, prompt: Text("studio-build"))
                    if let nameError {
                        Text(nameError).font(.callout).foregroundStyle(.red)
                    }
                } footer: {
                    Text("Shown in Nestor, where you assign workspaces to this runner.")
                        .foregroundStyle(.secondary)
                }
                Section("Resources") {
                    Toggle("Limit processor", isOn: $limitsCPU)
                    if limitsCPU {
                        Stepper("\(cpuCores) cores", value: $cpuCores, in: 1...64)
                    }
                    Toggle("Limit memory", isOn: $limitsMemory)
                    if limitsMemory {
                        Stepper("\(memoryGiB) GB", value: $memoryGiB, in: 1...512)
                    }
                    Stepper("\(concurrentJobs) job\(concurrentJobs == 1 ? "" : "s") at a time", value: $concurrentJobs, in: 1...16)
                }
                Section {
                    Toggle("Start the runner now", isOn: $startNow)
                } footer: {
                    Text(model.isDemo
                         ? "Demo: the runner is simulated. Nothing is installed and no credential is issued."
                         : "Nestor issues this runner's own credential; the service keeps it in the system keychain.")
                        .foregroundStyle(.secondary)
                }
                if let error {
                    Label(error, systemImage: "exclamationmark.triangle.fill")
                        .foregroundStyle(.red)
                }
            }
            .formStyle(.grouped)
            HStack {
                if isSubmitting { ProgressView().controlSize(.small) }
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button(model.isDemo ? "Add (Simulated)" : "Add Runner") { submit() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(isSubmitting || name.trimmingCharacters(in: .whitespaces).isEmpty || nameError != nil)
            }
            .padding(20)
        }
        .frame(width: 460)
    }

    private func submit() {
        let request = NewRunnerRequest(
            name: name,
            limits: ResourceLimits(
                cpuCores: limitsCPU ? cpuCores : nil,
                memoryBytes: limitsMemory ? UInt64(memoryGiB) << 30 : nil,
                concurrentJobs: concurrentJobs
            ),
            startNow: startNow
        )
        isSubmitting = true
        error = nil
        Task {
            defer { isSubmitting = false }
            do {
                let runner = try await model.addRunner(request)
                onAdded(runner)
                dismiss()
            } catch {
                self.error = (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
            }
        }
    }
}
