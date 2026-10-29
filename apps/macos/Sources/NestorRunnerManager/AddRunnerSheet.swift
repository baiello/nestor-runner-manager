import RunnerManagerKit
import SwiftUI

/// The add-runner flow. It asks for no secret: Nestor issues the runner's credential.
struct AddRunnerSheet: View {
    var model: RunnerManagerModel
    @State var form = AddRunnerForm()
    var onAdded: (Runner) -> Void
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        @Bindable var form = form
        VStack(alignment: .leading, spacing: 0) {
            Text("Add a Runner").font(.title2.bold()).padding([.top, .horizontal], 20)
            Form {
                Section {
                    TextField("Name", text: $form.name, prompt: Text("studio-build"))
                    if let nameError = form.nameError {
                        Text(nameError).font(.callout).foregroundStyle(.red)
                    }
                } footer: {
                    Text("Shown in Nestor, where you assign workspaces to this runner.")
                        .foregroundStyle(.secondary)
                }
                Section("Resources") {
                    Toggle("Limit processor", isOn: $form.limitsCPU)
                    if form.limitsCPU {
                        Stepper("\(form.cpuCores) cores", value: $form.cpuCores, in: 1...64)
                    }
                    Toggle("Limit memory", isOn: $form.limitsMemory)
                    if form.limitsMemory {
                        Stepper("\(form.memoryGiB) GB", value: $form.memoryGiB, in: 1...512)
                    }
                    Stepper("\(form.concurrentJobs) job\(form.concurrentJobs == 1 ? "" : "s") at a time", value: $form.concurrentJobs, in: 1...16)
                }
                Section {
                    Toggle("Start the runner now", isOn: $form.startNow)
                } footer: {
                    Text(model.isDemo
                         ? "Demo: the runner is simulated. Nothing is installed and no credential is issued."
                         : "Nestor issues this runner's own credential; the service keeps it in the system keychain.")
                        .foregroundStyle(.secondary)
                }
                if let error = form.error {
                    Label(error, systemImage: "exclamationmark.triangle.fill")
                        .foregroundStyle(.red)
                }
            }
            .formStyle(.grouped)
            .disabled(form.isSubmitting)
            HStack {
                if form.isSubmitting {
                    ProgressView().controlSize(.small)
                    Text("Adding the runner…").foregroundStyle(.secondary)
                }
                Spacer()
                // Disabled while sending: the runner may already exist, so cancelling could not undo it.
                Button("Cancel", role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                    .disabled(!form.canDismiss)
                Button(model.isDemo ? "Add (Simulated)" : "Add Runner") { submit() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(!form.canSubmit)
            }
            .padding(20)
        }
        .frame(width: 460)
        .interactiveDismissDisabled(!form.canDismiss)
    }

    private func submit() {
        Task {
            if let runner = await form.submit(to: model) {
                onAdded(runner)
                dismiss()
            }
        }
    }
}
