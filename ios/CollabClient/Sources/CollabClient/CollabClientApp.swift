import SwiftUI

@main
struct CollabClientApp: App {
    @StateObject private var vm = AppViewModel()

    var body: some Scene {
        WindowGroup {
            if vm.config.setupComplete {
                DashboardView(vm: vm)
                    .onAppear { vm.startDashboard() }
                    .onDisappear { vm.stopDashboard() }
            } else {
                SetupView(vm: vm)
            }
        }
    }
}
