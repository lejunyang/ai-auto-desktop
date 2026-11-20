#include <QAccessible>
#include <QApplication>
#include <QDialog>
#include <QLineEdit>
#include <QTextStream>
#include <QVBoxLayout>

static QLineEdit *field(QWidget &window, const QString &name, int x) {
    window.setWindowTitle(name);
    window.setAccessibleName(name);
    window.setGeometry(x, 100, 220, 90);
    auto *layout = new QVBoxLayout(&window);
    auto *entry = new QLineEdit(&window);
    // Identical names in three windows make a window-scoping regression observable.
    entry->setAccessibleName("Reference");
    layout->addWidget(entry);
    QObject::connect(entry, &QLineEdit::textChanged, [name](const QString &text) {
        QTextStream(stdout) << name << "=" << text << Qt::endl;
    });
    return entry;
}

int main(int argc, char **argv) {
    QApplication app(argc, argv);
    if (app.arguments().contains("ledger")) {
        app.setApplicationName("Mixed ledger fixture");
        QAccessible::setActive(true);
        QWidget ledger;
        field(ledger, "Ledger", 870);
        ledger.show();
        QTextStream(stdout) << "READY" << Qt::endl;
        return app.exec();
    }
    app.setApplicationName("Mixed recording fixture");
    QAccessible::setActive(true);
    QWidget main, sibling;
    QDialog dialog(&main);
    auto *entry = field(main, "Invoice", 30);
    field(sibling, "Unrelated", 650);
    field(dialog, "Approval", 330);
    QObject::connect(entry, &QLineEdit::textChanged, [&](const QString &value) {
        if (value == "N") dialog.show();
    });
    sibling.show();
    main.show();
    QTextStream(stdout) << "READY" << Qt::endl;
    return app.exec();
}
