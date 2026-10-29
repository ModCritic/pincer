using System.Windows;

namespace PincerTestHarness.Wpf;

public partial class OwnedPopupWindow : Window
{
    public OwnedPopupWindow() { InitializeComponent(); }
    private void OnCloseClick(object sender, RoutedEventArgs e) => Close();
}
