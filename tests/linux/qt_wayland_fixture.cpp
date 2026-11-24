#include <QApplication>
#include <QLineEdit>
#include <QPushButton>
#include <QScrollBar>
#include <QTextStream>
#include <QVBoxLayout>

static void receipt(const QString &value) {
    QTextStream(stdout) << value << '\n' << Qt::flush;
}

int main(int argc, char **argv) {
    QApplication app(argc, argv);
    if (!app.platformName().startsWith("wayland")) return 2;
    QWidget window;
    window.setWindowTitle("Qt directed input");
    QVBoxLayout layout(&window);
    QLineEdit source("selected invoice 2037");
    source.setAccessibleName("Invoice source");
    source.setDragEnabled(true);
    layout.addWidget(&source);
    QPushButton button("Submit invoice");
    QObject::connect(&button, &QPushButton::clicked, [] { receipt("CLICK"); });
    layout.addWidget(&button);
    QScrollBar bar(Qt::Horizontal);
    bar.setAccessibleName("Invoice scroll");
    bar.setValue(80);
    QObject::connect(&bar, &QScrollBar::valueChanged, [](int value) {
        receipt(QString("WHEEL %1").arg(value));
    });
    layout.addWidget(&bar);
    QObject::connect(&source, &QLineEdit::selectionChanged, [&] {
        receipt("SELECTION " + source.selectedText());
    });
    window.show();
    source.setFocus();
    receipt(QString("READY %1").arg(qVersion()));
    return app.exec();
}
