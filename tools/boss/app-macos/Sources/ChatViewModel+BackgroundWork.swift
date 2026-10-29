import Foundation

extension ChatViewModel {
    /// Badge count is the engine snapshot's length verbatim. The app must
    /// not filter or re-count by kind. The snapshot arrives only as an
    /// `engine.background_work` push (see `.backgroundWorkSnapshot`): the app
    /// neither polls for it nor derives it.
    var backgroundWorkVisibleCount: Int { backgroundWork.count }
}
