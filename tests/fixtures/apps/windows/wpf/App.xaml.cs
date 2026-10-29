using System;
using System.Windows;

namespace PincerTestHarness.Wpf;

public partial class App : Application
{
    // Ordinary launches show MainWindow exactly as before (it was the
    // StartupUri). PINCER_WPF_TASK_STATE selects the opt-in jev-use task window;
    // PINCER_WPF_TASK_DENSITY adds its distractor controls (#4312).
    private void OnStartup(object sender, StartupEventArgs e)
    {
        var taskState = Environment.GetEnvironmentVariable(TaskWindow.StateEnv);
        Window window = string.IsNullOrWhiteSpace(taskState)
            ? new MainWindow()
            : new TaskWindow(taskState, TaskWindow.DensityFromEnvironment());
        MainWindow = window;
        window.Show();
    }
}
