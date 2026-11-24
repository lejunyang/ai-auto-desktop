#include <QApplication>
#include <QDragEnterEvent>
#include <QLabel>
#include <QLineEdit>
#include <QPushButton>
#include <QScrollBar>
#include <QTextStream>
#include <QVBoxLayout>

static void receipt(const QString &value) {
    QTextStream(stdout) << value << '\n' << Qt::flush;
}

class RefusingDrop : public QLineEdit {
    void dragEnterEvent(QDragEnterEvent *event) override {
        receipt("REFUSED");
        event->ignore();
    }
};

class DropWindow : public QWidget {
    void dragEnterEvent(QDragEnterEvent *event) override {
        receipt("WRONG RECEIVER");
        event->acceptProposedAction();
    }
};

int main(int argc, char **argv) {
    QApplication app(argc, argv);
    if (!app.platformName().startsWith("wayland")) return 2;
    QWidget other;
    other.setWindowTitle("Qt directed input");
    QVBoxLayout otherLayout(&other);
    QLineEdit remote;
    remote.setAccessibleName("Other window receiver");
    otherLayout.addWidget(&remote);
    QObject::connect(&remote, &QLineEdit::textChanged, [](const QString &text) { receipt("REMOTE " + text); });
    other.show();
    DropWindow window;
    window.setAcceptDrops(true);
    window.setWindowTitle("Qt directed input");
    QVBoxLayout layout(&window);
    QLineEdit source("selected invoice 2037");
    source.setAccessibleName("Invoice source");
    source.setDragEnabled(true);
    layout.addWidget(&source);
    QLineEdit local;
    local.setAccessibleName("Same window receiver");
    QObject::connect(&local, &QLineEdit::textChanged, [](const QString &text) { receipt("LOCAL " + text); });
    layout.addWidget(&local);
    QLabel cover("cover", &local);
    cover.setGeometry(0, 0, 25, 32);
    RefusingDrop refusing;
    refusing.setAccessibleName("Refusing receiver");
    layout.addWidget(&refusing);
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
