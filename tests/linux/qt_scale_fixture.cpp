#include <QApplication>
#include <QAccessible>
#include <QLabel>
#include <QLineEdit>
#include <QKeyEvent>
#include <QPushButton>
#include <QTextStream>
#include <QTimer>
#include <QVBoxLayout>
#include <QWidget>

class ScaleInput : public QLineEdit {
public:
    explicit ScaleInput(QWidget *parent) : QLineEdit(parent) {}
    void keyPressEvent(QKeyEvent *event) override {
        if (event->key() == Qt::Key_F2) { setText("F2 received"); event->accept(); }
        else QLineEdit::keyPressEvent(event);
    }
};
int main(int argc, char **argv) {
    QApplication app(argc, argv);
    app.setApplicationName("AAD Scale Fixture");
    QAccessible::setActive(true);
    QWidget window;
    window.setWindowTitle("AAD Scale Fixture");
    window.setAccessibleName("AAD Scale Fixture");
    window.resize(420, 220);
    window.move(160, 180);
    auto *layout = new QVBoxLayout(&window);
    auto *button = new QPushButton("Scale click", &window);
    button->setAccessibleName("Scale click");
    auto *input = new ScaleInput(&window);
    input->setAccessibleName("Scale input");
    auto *status = new QLabel("Scale clicks 0", &window);
    status->setAccessibleName("Scale clicks 0");
    layout->addWidget(button); layout->addWidget(input); layout->addWidget(status);
    int count = 0;
    QObject::connect(button, &QPushButton::clicked, [&] {
        const auto text = QString("Scale clicks %1").arg(++count);
        status->setText(text); status->setAccessibleName(text);
    });
    window.show();
    QWidget extra;
    if (app.arguments().contains("--ambiguous")) {
        extra.setWindowTitle("AAD Other Scale Window");
        extra.setAccessibleName("AAD Scale Fixture");
        extra.resize(420, 220);
        extra.move(700, 180);
        extra.show();
    }
    QTimer::singleShot(400, [&] {
        window.raise(); window.activateWindow(); input->setFocus();
        const auto origin = window.mapToGlobal(QPoint(0,0));
        QTextStream(stdout) << "{\"ready\":true,\"scale\":" << window.devicePixelRatioF()
            << ",\"x\":" << origin.x() << ",\"y\":" << origin.y()
            << ",\"width\":" << window.width() << ",\"height\":" << window.height()
            << ",\"xid\":" << window.winId() << "}\n" << Qt::flush;
    });
    return app.exec();
}
