#include <QAccessible>
#include <QApplication>
#include <QImage>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLineEdit>
#include <QPainter>
#include <QPushButton>
#include <QTextStream>
#include <QTimer>

class Surface : public QPushButton {
public:
    explicit Surface(QWidget *parent) : QPushButton(parent) {
        setAccessibleName("Capture surface");
        setGeometry(20, 20, 240, 140);
    }
protected:
    void paintEvent(QPaintEvent *) override {
        QPainter painter(this);
        painter.fillRect(rect(), QColor(51, 170, 85));
        painter.setPen(Qt::black);
        painter.drawText(rect(), Qt::AlignCenter, "Capture surface");
    }
};

int main(int argc, char **argv) {
    if (argc == 3 && QString::fromLocal8Bit(argv[1]) == "--verify-png") {
        QCoreApplication app(argc, argv);
        const QImage image(QString::fromLocal8Bit(argv[2]));
        if (image.isNull()) return 2;
        int green = 0;
        for (int y = 0; y < image.height(); ++y) {
            for (int x = 0; x < image.width(); ++x) {
                const QColor color = image.pixelColor(x, y);
                if (qAbs(color.red() - 51) <= 2 && qAbs(color.green() - 170) <= 2 && qAbs(color.blue() - 85) <= 2) ++green;
            }
        }
        const QJsonObject result{{"width", image.width()}, {"height", image.height()},
            {"green_fraction", double(green) / (image.width() * image.height())}};
        QTextStream(stdout) << QJsonDocument(result).toJson(QJsonDocument::Compact) << '\n';
        return 0;
    }
    QApplication app(argc, argv);
    QAccessible::setActive(true);
    app.setApplicationName("AAD Scoped Capture Fixture");
    QWidget window;
    window.setWindowTitle("AAD Scoped Capture Fixture");
    window.setAccessibleName("AAD Scoped Capture Fixture");
    window.setFixedSize(520, 220);
    new Surface(&window);
    const QString mode = app.arguments().value(1, "clear");
    if (mode == "overlay") {
        auto *overlay = new QPushButton("Sibling", &window);
        overlay->setAccessibleName("Covering sibling");
        overlay->setEnabled(false);
        // Cover a corner: checking just the surface center still succeeds.
        overlay->setGeometry(25, 25, 70, 35);
    } else if (mode == "protected" || mode == "protected-outside") {
        auto *secret = new QLineEdit(&window);
        secret->setAccessibleName("Protected sibling");
        secret->setEchoMode(QLineEdit::Password);
        secret->setText("synthetic-only");
        secret->setGeometry(mode == "protected" ? 25 : 300, 25, 80, 35);
    }
    window.show();
    QTimer::singleShot(300, [] { QTextStream(stdout) << "READY\n" << Qt::flush; });
    return app.exec();
}
