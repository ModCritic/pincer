// PincerTestHarness.Qt6: the Pincer Qt Probe.
//
// A Qt 6 Widgets app shaped like the local PySide6 app whose testing
// turned up the Qt issues this probe reproduces: combo box popups, a large
// accessible tree of month tables, a modal import dialog that opens a portal
// file dialog, a top-level window and message boxes stacked over that modal,
// a QInputDialog, and a masked password field.
//
// Every user-visible effect is appended as one line to the file named by
// PROBE_LOG (default /tmp/pincer-qt-probe.log), so a test can check what
// happened without a screenshot. Lines look like
//   2026-11-09T00:00:00.000 pid=1234 button "Show receipt"
// and the text after the pid is stable for grepping.
//
// Environment:
//   PROBE_LOG     log file path (default /tmp/pincer-qt-probe.log)
//   PROBE_TABLES  number of month tables in the main window (default 12)
//   PROBE_ROWS    rows per table (default 16)
//   PROBE_COLS    columns per table (default 10)
//
// With the defaults the main window's tree is about 2,300 AT-SPI nodes, enough
// that a first get_window_state walk runs past its 2000 ms budget.
//
// File dialog: the probe sets PLASMA_INTEGRATION_USE_PORTAL=1 unless it is
// already set, so the KDE platform theme sends "Browse..." through
// xdg-desktop-portal and the file dialog belongs to xdg-desktop-portal-kde, a
// different process, as it did for the PySide6 app (whose bundled Qt has only
// the xdgdesktopportal theme). Set PLASMA_INTEGRATION_USE_PORTAL=0 for the
// in-process KDE file dialog instead.
//
// Accessibility: Qt turns its AT-SPI bridge on when the a11y bus reports
// org.a11y.Status.IsEnabled, or always when QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1.

#include <QApplication>
#include <QComboBox>
#include <QDateTime>
#include <QDialog>
#include <QDialogButtonBox>
#include <QFile>
#include <QFileDialog>
#include <QFormLayout>
#include <QGridLayout>
#include <QGroupBox>
#include <QHBoxLayout>
#include <QHeaderView>
#include <QInputDialog>
#include <QLabel>
#include <QLineEdit>
#include <QMainWindow>
#include <QMenuBar>
#include <QMessageBox>
#include <QPushButton>
#include <QScrollArea>
#include <QTableWidget>
#include <QVBoxLayout>
#include <QCloseEvent>

static QString g_logPath;

static void logLine(const QString &what)
{
    QFile f(g_logPath);
    if (!f.open(QIODevice::Append | QIODevice::Text))
        return;
    const QString line = QStringLiteral("%1 pid=%2 %3\n")
                             .arg(QDateTime::currentDateTime().toString(Qt::ISODateWithMs))
                             .arg(QCoreApplication::applicationPid())
                             .arg(what);
    f.write(line.toUtf8());
}

static QString quoted(const QString &s)
{
    return QLatin1Char('"') + s + QLatin1Char('"');
}

static int envInt(const char *name, int fallback)
{
    bool ok = false;
    const int v = qEnvironmentVariableIntValue(name, &ok);
    return ok && v >= 0 ? v : fallback;
}

static QPushButton *loggedButton(const QString &text, QWidget *parent)
{
    auto *b = new QPushButton(text, parent);
    QObject::connect(b, &QPushButton::clicked, b, [text] { logLine(QStringLiteral("button ") + quoted(text)); });
    return b;
}

// A top-level window parented to the import dialog, so it stacks over it.
// A QMainWindow, so AT-SPI reports it as a frame like any app window.
class ReceiptViewer : public QMainWindow
{
public:
    explicit ReceiptViewer(QWidget *parent) : QMainWindow(parent)
    {
        setAttribute(Qt::WA_DeleteOnClose);
        setWindowTitle(QStringLiteral("Receipt viewer"));
        auto *body = new QWidget(this);
        auto *lay = new QVBoxLayout(body);
        lay->addWidget(new QLabel(QStringLiteral("Receipt 0001: Groceries, 42.17"), body));
        auto *close = loggedButton(QStringLiteral("Close receipt"), body);
        connect(close, &QPushButton::clicked, this, &QWidget::close);
        lay->addWidget(close);
        setCentralWidget(body);
        resize(360, 200);
        logLine(QStringLiteral("window \"Receipt viewer\" opened"));
    }

protected:
    void closeEvent(QCloseEvent *e) override
    {
        logLine(QStringLiteral("window \"Receipt viewer\" closed"));
        QMainWindow::closeEvent(e);
    }
};

class ImportDialog : public QDialog
{
public:
    explicit ImportDialog(QWidget *parent) : QDialog(parent)
    {
        setWindowTitle(QStringLiteral("Import scans"));
        setModal(true);
        auto *lay = new QVBoxLayout(this);
        auto *form = new QFormLayout;

        auto *kind = new QComboBox(this);
        kind->setAccessibleName(QStringLiteral("Scan type"));
        kind->addItems({QStringLiteral("Receipt"), QStringLiteral("Invoice"), QStringLiteral("Bank statement"),
                        QStringLiteral("Pay stub"), QStringLiteral("Other")});
        connect(kind, &QComboBox::currentTextChanged, this,
                [](const QString &t) { logLine(QStringLiteral("combo \"Scan type\" = ") + quoted(t)); });
        form->addRow(QStringLiteral("Scan type"), kind);

        auto *fileRow = new QHBoxLayout;
        m_path = new QLineEdit(this);
        m_path->setAccessibleName(QStringLiteral("Scan file"));
        m_path->setReadOnly(true);
        auto *browse = loggedButton(QStringLiteral("Browse..."), this);
        connect(browse, &QPushButton::clicked, this, [this] {
            const QString f = QFileDialog::getOpenFileName(this, QStringLiteral("Choose a scan"), QString(),
                                                           QStringLiteral("Images (*.png *.jpg *.pdf);;All files (*)"));
            if (f.isEmpty()) {
                logLine(QStringLiteral("file dialog canceled"));
            } else {
                m_path->setText(f);
                logLine(QStringLiteral("file dialog chose ") + quoted(f));
            }
        });
        fileRow->addWidget(m_path);
        fileRow->addWidget(browse);
        form->addRow(QStringLiteral("Scan file"), fileRow);

        auto *pw = new QLineEdit(this);
        pw->setAccessibleName(QStringLiteral("Password"));
        pw->setEchoMode(QLineEdit::Password);
        auto *save = loggedButton(QStringLiteral("Save"), this);
        connect(save, &QPushButton::clicked, this,
                [pw] { logLine(QStringLiteral("password saved length=%1").arg(pw->text().size())); });
        auto *pwRow = new QHBoxLayout;
        pwRow->addWidget(pw);
        pwRow->addWidget(save);
        form->addRow(QStringLiteral("Password"), pwRow);
        lay->addLayout(form);

        auto *actions = new QHBoxLayout;
        auto *receipt = loggedButton(QStringLiteral("Show receipt"), this);
        connect(receipt, &QPushButton::clicked, this, [this] { (new ReceiptViewer(this))->show(); });
        auto *warn = loggedButton(QStringLiteral("Warn"), this);
        connect(warn, &QPushButton::clicked, this, [this] {
            const auto r = QMessageBox::warning(this, QStringLiteral("Probe warning"),
                                                QStringLiteral("This is the probe's warning message."),
                                                QMessageBox::Ok | QMessageBox::Cancel);
            logLine(QStringLiteral("message box closed with ") + (r == QMessageBox::Ok ? "Ok" : "Cancel"));
        });
        auto *person = loggedButton(QStringLiteral("Add person"), this);
        connect(person, &QPushButton::clicked, this, [this] {
            bool ok = false;
            const QString name = QInputDialog::getText(this, QStringLiteral("Add person"), QStringLiteral("Name:"),
                                                       QLineEdit::Normal, QString(), &ok);
            if (ok)
                logLine(QStringLiteral("input accepted text=") + quoted(name));
            else
                logLine(QStringLiteral("input canceled"));
        });
        actions->addWidget(receipt);
        actions->addWidget(warn);
        actions->addWidget(person);
        lay->addLayout(actions);

        auto *box = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel, this);
        connect(box, &QDialogButtonBox::accepted, this, &QDialog::accept);
        connect(box, &QDialogButtonBox::rejected, this, &QDialog::reject);
        lay->addWidget(box);
        resize(520, 260);
    }

private:
    QLineEdit *m_path = nullptr;
};

class ProbeWindow : public QMainWindow
{
public:
    ProbeWindow()
    {
        setWindowTitle(QStringLiteral("Pincer Qt Probe"));

        auto *file = menuBar()->addMenu(QStringLiteral("&File"));
        auto *openImport = file->addAction(QStringLiteral("Open import..."));
        connect(openImport, &QAction::triggered, this, [this] {
            logLine(QStringLiteral("menu File > \"Open import...\""));
            openImportDialog();
        });
        file->addSeparator();
        auto *quit = file->addAction(QStringLiteral("&Quit"));
        quit->setShortcut(QKeySequence(Qt::CTRL | Qt::Key_Q));
        connect(quit, &QAction::triggered, this, [this] {
            logLine(QStringLiteral("menu File > \"Quit\""));
            close();
        });
        auto *help = menuBar()->addMenu(QStringLiteral("&Help"));
        connect(help->addAction(QStringLiteral("About")), &QAction::triggered, this,
                [] { logLine(QStringLiteral("menu Help > \"About\"")); });

        auto *central = new QWidget(this);
        auto *lay = new QVBoxLayout(central);
        auto *top = new QHBoxLayout;
        top->addWidget(new QLabel(QStringLiteral("Category"), central));
        auto *category = new QComboBox(central);
        category->setAccessibleName(QStringLiteral("Category"));
        category->addItems({QStringLiteral("Groceries"), QStringLiteral("Rent"), QStringLiteral("Utilities"),
                            QStringLiteral("Transport"), QStringLiteral("Dining"), QStringLiteral("Health"),
                            QStringLiteral("Gifts"), QStringLiteral("Other")});
        connect(category, &QComboBox::currentTextChanged, this,
                [](const QString &t) { logLine(QStringLiteral("combo \"Category\" = ") + quoted(t)); });
        top->addWidget(category);
        auto *open = loggedButton(QStringLiteral("Open import..."), central);
        connect(open, &QPushButton::clicked, this, &ProbeWindow::openImportDialog);
        top->addWidget(open);
        top->addStretch(1);
        lay->addLayout(top);

        // The bulk of the accessible tree: one table per month, every cell exposed.
        const int tables = envInt("PROBE_TABLES", 12);
        const int rows = envInt("PROBE_ROWS", 16);
        const int cols = envInt("PROBE_COLS", 10);
        static const char *months[] = {"January", "February", "March",     "April",   "May",      "June",
                                       "July",    "August",   "September", "October", "November", "December"};
        static const char *headers[] = {"Date", "Payee", "Category", "Amount", "Account",
                                        "Method", "Note", "Tag", "Status", "Balance"};
        auto *grid = new QGridLayout;
        auto *gridHost = new QWidget;
        gridHost->setLayout(grid);
        for (int t = 0; t < tables; ++t) {
            const QString name = t < 12 ? QString::fromLatin1(months[t]) : QStringLiteral("Month %1").arg(t + 1);
            auto *group = new QGroupBox(name, gridHost);
            auto *glay = new QVBoxLayout(group);
            auto *table = new QTableWidget(rows, cols, group);
            table->setAccessibleName(name + QStringLiteral(" table"));
            QStringList hdr;
            for (int c = 0; c < cols; ++c)
                hdr << (c < 10 ? QString::fromLatin1(headers[c]) : QStringLiteral("Col %1").arg(c + 1));
            table->setHorizontalHeaderLabels(hdr);
            for (int r = 0; r < rows; ++r)
                for (int c = 0; c < cols; ++c)
                    table->setItem(r, c, new QTableWidgetItem(QStringLiteral("%1 %2.%3").arg(name.left(3)).arg(r + 1).arg(c + 1)));
            table->setMinimumHeight(160);
            glay->addWidget(table);
            grid->addWidget(group, t / 4, t % 4);
        }
        auto *scroll = new QScrollArea(central);
        scroll->setWidgetResizable(true);
        scroll->setWidget(gridHost);
        lay->addWidget(scroll, 1);

        setCentralWidget(central);
        resize(1200, 800);
        logLine(QStringLiteral("start tables=%1 rows=%2 cols=%3").arg(tables).arg(rows).arg(cols));
    }

    void openImportDialog()
    {
        ImportDialog dlg(this);
        logLine(QStringLiteral("dialog \"Import scans\" opened"));
        const int r = dlg.exec();
        logLine(QStringLiteral("dialog \"Import scans\" ") + (r == QDialog::Accepted ? "accepted" : "rejected"));
    }

protected:
    void closeEvent(QCloseEvent *e) override
    {
        logLine(QStringLiteral("window \"Pincer Qt Probe\" closed"));
        QMainWindow::closeEvent(e);
    }
};

int main(int argc, char **argv)
{
    // Read by the KDE platform theme while QApplication starts.
    if (!qEnvironmentVariableIsSet("PLASMA_INTEGRATION_USE_PORTAL"))
        qputenv("PLASMA_INTEGRATION_USE_PORTAL", "1");
    QApplication app(argc, argv);
    QApplication::setApplicationName(QStringLiteral("PincerQtProbe"));
    g_logPath = qEnvironmentVariable("PROBE_LOG", QStringLiteral("/tmp/pincer-qt-probe.log"));
    ProbeWindow w;
    w.show();
    return app.exec();
}
