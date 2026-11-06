#include <QApplication>
#include <QAccessible>
#include <QDrag>
#include <QDragEnterEvent>
#include <QDropEvent>
#include <QLabel>
#include <QMimeData>
#include <QMouseEvent>
#include <QPushButton>
#include <QTextStream>
#include <QTimer>

class CrossSurface : public QPushButton {
public:
    CrossSurface(QWidget *parent,QLabel *status,bool source,QString mode)
        :QPushButton(source?"Cross source":"Cross destination",parent),status_(status),source_(source),mode_(std::move(mode)) {
        setAccessibleName(text());setGeometry(0,0,300,180);setAcceptDrops(!source_);setMouseTracking(true);
    }
protected:
    void enterEvent(QEvent *event) override {
        QPushButton::enterEvent(event);
        if(source_&&mode_=="hover"&&!covered_) {
            covered_=true;
            QTimer::singleShot(0,this,[this]{
                auto *cover=new QPushButton("Cross hover obstacle",parentWidget());
                cover->setAccessibleName("Cross hover obstacle");cover->setGeometry(geometry());cover->setEnabled(false);cover->show();cover->raise();
                QTextStream(stdout)<<"HOVER\n"<<Qt::flush;
            });
        }
    }
    void mousePressEvent(QMouseEvent *event) override {
        if(!source_)return;
        origin_=event->pos();pressed_=true;status_->setText("Cross source pressed");status_->setAccessibleName(status_->text());
        QTextStream(stdout)<<"PRESS\n"<<Qt::flush;
        if(mode_=="geometry")QTimer::singleShot(1,this,[this]{window()->move(window()->pos()+QPoint(20,0));});
        event->accept();
    }
    void mouseMoveEvent(QMouseEvent *event) override {
        if(!source_||!pressed_||!(event->buttons()&Qt::LeftButton)||(event->pos()-origin_).manhattanLength()<QApplication::startDragDistance())return;
        pressed_=false;auto *mime=new QMimeData;mime->setText("AAD cross-process drag fixture");
        auto *drag=new QDrag(this);drag->setMimeData(mime);
        const auto result=drag->exec(Qt::CopyAction|Qt::MoveAction,Qt::CopyAction);
        status_->setText(result==Qt::IgnoreAction?"Cross source cancelled":"Cross source accepted");status_->setAccessibleName(status_->text());
        QTextStream(stdout)<<"DONE "<<(result==Qt::IgnoreAction?"cancelled":"accepted")<<'\n'<<Qt::flush;
    }
    void mouseReleaseEvent(QMouseEvent *event) override {
        if(source_&&pressed_){pressed_=false;status_->setText("Cross source released");status_->setAccessibleName(status_->text());QTextStream(stdout)<<"UP\n"<<Qt::flush;}
        event->accept();
    }
    void dragEnterEvent(QDragEnterEvent *event) override {
        if(!source_&&event->mimeData()->text()=="AAD cross-process drag fixture")event->acceptProposedAction();
    }
    void dragMoveEvent(QDragMoveEvent *event) override {if(!source_)event->acceptProposedAction();}
    void dropEvent(QDropEvent *event) override {
        QTextStream(stdout)<<"DROP begin\n"<<Qt::flush;
        if(!source_&&event->mimeData()->text()=="AAD cross-process drag fixture"){
            event->acceptProposedAction();status_->setText("Cross destination received");status_->setAccessibleName(status_->text());
            QTextStream(stdout)<<"DROP matched\n"<<Qt::flush;
        }
    }
private:
    QLabel *status_;bool source_,pressed_=false,covered_=false;QString mode_;QPoint origin_;
};

int main(int argc,char **argv) {
    QApplication::setAttribute(Qt::AA_DisableHighDpiScaling);
    QApplication app(argc,argv);QAccessible::setActive(true);
    const bool source=app.arguments().value(1)=="source";const QString mode=app.arguments().value(2,"success");
    app.setApplicationName(source?"AAD Cross Source":"AAD Cross Destination");app.setOrganizationName("ai-auto-desktop");
    QWidget window(nullptr,Qt::Window|Qt::FramelessWindowHint);window.setWindowTitle(app.applicationName());window.setAccessibleName(app.applicationName());
    window.setFixedSize(300,220);window.move(source?100:400,mode=="hover"?450:100);
    auto *status=new QLabel(source?"Cross source idle":"Cross destination idle",&window);status->setAccessibleName(status->text());status->setGeometry(5,185,290,30);
    new CrossSurface(&window,status,source,mode);window.show();window.raise();
    QTextStream(stdout)<<"READY\n"<<Qt::flush;return app.exec();
}
