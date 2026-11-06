#include <QApplication>
#include <QAccessible>
#include <QElapsedTimer>
#include <QKeyEvent>
#include <QLabel>
#include <QLineEdit>
#include <QMouseEvent>
#include <QPushButton>
#include <QTextStream>
#include <QVBoxLayout>
#include <QWidget>

// Only timings leave the fixture; no key codes, characters or input text.
bool report_events = false;
void report(const QString &event) {
    if (report_events) QTextStream(stdout) << event << '\n' << Qt::flush;
}
void status(QLabel *label, const QString &value) {
    label->setText(value);
    label->setAccessibleName(value);
}

class HoldKeyTarget : public QLineEdit {
public:
    HoldKeyTarget(QLabel *label, QWidget *parent) : QLineEdit(parent), label_(label) {
        setAccessibleName("Hold key target");
    }
protected:
    void keyPressEvent(QKeyEvent *event) override {
        if (!event->isAutoRepeat() && !held_) { held_ = true; clock_.start(); report("key down"); }
        event->accept();
    }
    void keyReleaseEvent(QKeyEvent *event) override {
        if (!event->isAutoRepeat() && held_) {
            held_ = false;
            status(label_, QString("Hold key released %1 ms").arg(clock_.elapsed()));
            report(QString("key up %1").arg(clock_.elapsed()));
        }
        event->accept();
    }
private:
    QLabel *label_;
    QElapsedTimer clock_;
    bool held_ = false;
};

class HoldClickTarget : public QPushButton {
public:
    HoldClickTarget(QLabel *label, QWidget *parent) : QPushButton("Hold click target", parent), label_(label) {
        setAccessibleName("Hold click target");
    }
protected:
    void mousePressEvent(QMouseEvent *event) override {
        clock_.start();
        report("click down");
        QPushButton::mousePressEvent(event);
    }
    void mouseReleaseEvent(QMouseEvent *event) override {
        if (clock_.isValid()) status(label_, QString("Hold click released %1 ms").arg(clock_.elapsed()));
        if (clock_.isValid()) report(QString("click up %1").arg(clock_.elapsed()));
        QPushButton::mouseReleaseEvent(event);
    }
private:
    QLabel *label_;
    QElapsedTimer clock_;
};

class HoldDragTarget : public QPushButton {
public:
    HoldDragTarget(QLabel *label, QWidget *parent) : QPushButton("Hold drag surface", parent), label_(label) {
        setAccessibleName("Hold drag surface");
        setMinimumHeight(100);
    }
protected:
    void mousePressEvent(QMouseEvent *event) override {
        clock_.start(); first_move_ms_ = -1; start_ = event->pos();
        report("drag down");
        event->accept();
    }
    void mouseMoveEvent(QMouseEvent *event) override {
        if (clock_.isValid() && first_move_ms_ < 0 && event->buttons().testFlag(Qt::LeftButton) && event->pos() != start_) {
            first_move_ms_ = clock_.elapsed();
        }
        event->accept();
    }
    void mouseReleaseEvent(QMouseEvent *event) override {
        if (clock_.isValid()) status(label_, QString("Hold drag released %1 ms first move %2 ms").arg(clock_.elapsed()).arg(first_move_ms_));
        if (clock_.isValid()) report(QString("drag up %1").arg(clock_.elapsed()));
        event->accept();
    }
private:
    QLabel *label_;
    QElapsedTimer clock_;
    QPoint start_;
    qint64 first_move_ms_ = -1;
};

int main(int argc, char **argv) {
    QApplication application(argc, argv);
    report_events = application.arguments().contains("--report-events");
    QAccessible::setActive(true);
    application.setApplicationName("AAD Qt Hold Fixture");
    application.setOrganizationName("ai-auto-desktop");
    QWidget window;
    window.setWindowTitle("AAD Qt Hold Fixture");
    window.setAccessibleName("AAD Qt Hold Fixture");
    window.resize(480, 360);
    auto *layout = new QVBoxLayout(&window);
    auto *key_status = new QLabel("Hold key idle", &window);
    auto *key = new HoldKeyTarget(key_status, &window);
    layout->addWidget(key); layout->addWidget(key_status);
    auto *click_status = new QLabel("Hold click idle", &window);
    layout->addWidget(new HoldClickTarget(click_status, &window)); layout->addWidget(click_status);
    auto *drag_status = new QLabel("Hold drag idle", &window);
    layout->addWidget(new HoldDragTarget(drag_status, &window)); layout->addWidget(drag_status);
    window.show(); window.raise(); window.activateWindow(); key->setFocus();
    QTextStream(stdout) << "READY\n" << Qt::flush;
    return application.exec();
}
