import Foundation
import Observation

/// The state of the add-runner flow.
///
/// A creation cannot be called back once sent: the service may already have created the runner.
/// So while it is in flight the form cannot be dismissed, and Cancel stays disabled until the
/// service has answered.
@MainActor
@Observable
public final class AddRunnerForm {
    public var name = ""
    public var limitsCPU = false
    public var cpuCores = 4
    public var limitsMemory = false
    public var memoryGiB = 16
    public var startNow = true

    public private(set) var isSubmitting = false
    /// Why the last submission failed, shown in the form.
    public private(set) var error: String?
    /// The runner created by the last submission.
    public private(set) var created: Runner?

    public init() {}

    /// Whether the sheet may be closed, by Cancel, Escape or otherwise.
    public var canDismiss: Bool { !isSubmitting }

    /// Why the name is not acceptable, once something has been typed.
    public var nameError: String? {
        guard !name.isEmpty else { return nil }
        do {
            _ = try RunnerNameRule.validate(name)
            return nil
        } catch {
            return RunnerManagerModel.message(for: error)
        }
    }

    public var canSubmit: Bool {
        !isSubmitting && !name.trimmingCharacters(in: .whitespaces).isEmpty && nameError == nil
    }

    public var request: NewRunnerRequest {
        NewRunnerRequest(
            name: name,
            limits: ResourceLimits(
                cpuCores: limitsCPU ? cpuCores : nil,
                memoryBytes: limitsMemory ? UInt64(memoryGiB) << 30 : nil
            ),
            startNow: startNow
        )
    }

    /// Sends the request once; returns the created runner, or nil if it failed or was already sending.
    @discardableResult
    public func submit(to model: RunnerManagerModel) async -> Runner? {
        guard canSubmit else { return nil }
        isSubmitting = true
        error = nil
        defer { isSubmitting = false }
        do {
            let runner = try await model.addRunner(request)
            created = runner
            return runner
        } catch {
            self.error = RunnerManagerModel.message(for: error)
            return nil
        }
    }
}
