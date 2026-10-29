using System.Windows;

namespace PincerTestHarness.Wpf;

public partial class LayeredPopupWindow : Window
{
    public LayeredPopupWindow() { InitializeComponent(); }
    private void OnCloseClick(object sender, RoutedEventArgs e) => Close();
}
