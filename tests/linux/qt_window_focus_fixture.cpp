#include <QApplication>
#include <QAccessible>
#include <QAccessibleWidget>
#include <QKeyEvent>
#include <QLabel>
#include <QTextStream>
#include <QTimer>
#include <QVBoxLayout>
#include <QWidget>

// Deliberately emulate a toolkit with a stale Focused bit and a no-op focus
// action. The independent counters measure actual X11 input delivery.
class KeyCounter : public QWidget {
public:
    KeyCounter(const QString &name, QLabel *status, QWidget *parent)
        : QWidget(parent), status_(status), name_(name) {
        if (name == "Stale primary target") setObjectName("aad-stale-focus");
        setAccessibleName(name); setFocusPolicy(Qt::StrongFocus); setMinimumSize(240, 70);
        updateStatus();
    }
protected:
    void keyPressEvent(QKeyEvent *event) override {
        if (!event->isAutoRepeat() && event->key() == Qt::Key_F2) { ++presses_; updateStatus(); }
        event->accept();
    }
    void keyReleaseEvent(QKeyEvent *event) override {
        if (!event->isAutoRepeat() && event->key() == Qt::Key_F2) { ++releases_; updateStatus(); }
        event->accept();
    }
private:
    void updateStatus() {
        const QString text = QString("%1 delivered %2 released %3").arg(name_).arg(presses_).arg(releases_);
        status_->setText(text); status_->setAccessibleName(text);
    }
    QLabel *status_; QString name_; int presses_ = 0, releases_ = 0;
};

class StaleFocusAccessible : public QAccessibleWidget {
public:
    explicit StaleFocusAccessible(QWidget *widget) : QAccessibleWidget(widget, QAccessible::EditableText) {}
    QAccessible::State state() const override {
        auto value = QAccessibleWidget::state(); value.focusable = true; value.focused = true; return value;
    }
    void doAction(const QString &name) override {
        if (name != QAccessibleActionInterface::setFocusAction()) QAccessibleWidget::doAction(name);
    }
};

int main(int argc, char **argv) {
    QApplication app(argc, argv);
    app.setApplicationName("AAD Window Focus Fixture");
    QAccessible::installFactory([](const QString &, QObject *object) -> QAccessibleInterface * {
        auto *widget = qobject_cast<QWidget *>(object);
        if (widget && widget->objectName() == "aad-stale-focus") return new StaleFocusAccessible(widget);
        return nullptr;
    });
    QAccessible::setActive(true);
    QWidget primary, secondary;
    primary.setWindowTitle("AAD Primary Window"); primary.setAccessibleName("AAD Primary Window");
    secondary.setWindowTitle("AAD Secondary Window"); secondary.setAccessibleName("AAD Secondary Window");
    primary.resize(360, 160); primary.move(200, 300);
    secondary.resize(360, 160); secondary.move(650, 300);
    auto *primaryLayout = new QVBoxLayout(&primary);
    auto *primaryStatus = new QLabel(&primary);
    auto *stale = new KeyCounter("Stale primary target", primaryStatus, &primary);
    stale->setObjectName("aad-stale-focus");
    primaryLayout->addWidget(stale); primaryLayout->addWidget(primaryStatus);
    auto *secondaryLayout = new QVBoxLayout(&secondary);
    auto *secondaryStatus = new QLabel(&secondary);
    auto *live = new KeyCounter("Live secondary target", secondaryStatus, &secondary);
    secondaryLayout->addWidget(live); secondaryLayout->addWidget(secondaryStatus);
    primary.show(); secondary.show();
    QTimer::singleShot(250, [&] {
        secondary.raise(); secondary.activateWindow(); live->setFocus();
        QTextStream(stdout) << "READY\n" << Qt::flush;
    });
    return app.exec();
}
