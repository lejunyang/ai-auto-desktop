#include <QApplication>
#include <QAccessible>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QMouseEvent>
#include <QPainter>
#include <QPushButton>
#include <QTextStream>
#include <QTimer>

class PathSurface : public QPushButton {
public:
    PathSurface(QWidget *parent, QLabel *status, const QString &mode)
        : QPushButton("Path surface", parent), status_(status), mode_(mode) {
        setAccessibleName("Path surface");
        setGeometry(20, 20, 600, 300);
    }
protected:
    void mousePressEvent(QMouseEvent *event) override {
        held_ = true; trace_.clear(); trace_.push_back(event->pos());
        status_->setText("Path down"); status_->setAccessibleName("Path down");
        emitEvent("down", event->pos());
        if (mode_ == "geometry") QTimer::singleShot(1, this, [this] { window()->move(window()->pos() + QPoint(20,0)); });
        if (mode_ == "overlay") QTimer::singleShot(1, this, [this] {
            auto *overlay = new QPushButton("Path obstacle", parentWidget());
            overlay->setAccessibleName("Path obstacle"); overlay->setGeometry(geometry()); overlay->show(); overlay->raise();
        });
        event->accept();
    }
    void mouseMoveEvent(QMouseEvent *event) override {
        if (held_ && event->buttons().testFlag(Qt::LeftButton)) {
            trace_.push_back(event->pos()); emitEvent("move", event->pos()); update();
        }
        event->accept();
    }
    void mouseReleaseEvent(QMouseEvent *event) override {
        held_ = false; emitEvent("up", event->pos());
        const auto text = QString("Path released moves %1").arg(trace_.size() - 1);
        status_->setText(text); status_->setAccessibleName(text); event->accept();
    }
    void paintEvent(QPaintEvent *event) override {
        QPushButton::paintEvent(event);
        QPainter painter(this); painter.setPen(QPen(Qt::blue, 3));
        for (int i=1; i<trace_.size(); ++i) painter.drawLine(trace_[i-1], trace_[i]);
    }
private:
    void emitEvent(const QString &kind, const QPoint &pos) {
        const QJsonObject value{{"event",kind},{"fx",double(pos.x())/width()},
            {"fy",double(pos.y())/height()},{"held",held_}};
        QTextStream(stdout) << "PATH " << QJsonDocument(value).toJson(QJsonDocument::Compact) << '\n' << Qt::flush;
    }
    QLabel *status_; QString mode_; QVector<QPoint> trace_; bool held_ = false;
};

int main(int argc, char **argv) {
    QApplication app(argc, argv); QAccessible::setActive(true);
    app.setApplicationName("AAD Path Fixture"); app.setOrganizationName("ai-auto-desktop");
    const QString mode = app.arguments().value(1, "success");
    QWidget window; window.setWindowTitle("AAD Path Fixture"); window.setAccessibleName("AAD Path Fixture"); window.setFixedSize(640,400);
    auto *status = new QLabel("Path idle", &window); status->setAccessibleName("Path idle"); status->setGeometry(20,340,600,40);
    auto *surface = new PathSurface(&window,status,mode);
    window.show(); window.raise(); window.activateWindow(); surface->setFocus();
    QTextStream(stdout) << "READY\n" << Qt::flush;
    return app.exec();
}
