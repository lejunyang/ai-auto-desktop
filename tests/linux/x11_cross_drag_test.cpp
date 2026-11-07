// Private X server only: two actual X clients/PIDs, independently observed
// held pointer events, and hostile route windows that have no input authority.
#define main aad_xtest_helper_main
#include "../../plugins/linux_atspi/x11_xtest_helper.cpp"
#undef main
#include <stdexcept>

static void CrossRequire(bool condition,const char *message) { if(!condition)throw std::runtime_error(message); }
struct ChildOwner { pid_t pid=-1; ~ChildOwner(){if(pid>0){kill(pid,SIGTERM);waitpid(pid,nullptr,0);}} };

int main() {
    try {
        setenv("XDG_SESSION_TYPE","x11",1);
        Display *display=XOpenDisplay(nullptr);CrossRequire(display,"private X display unavailable");
        XSetErrorHandler(HandleXError);
        const Window root=DefaultRootWindow(display);
        const Atom pid_atom=XInternAtom(display,"_NET_WM_PID",False);
        const Atom check=XInternAtom(display,"_NET_SUPPORTING_WM_CHECK",False);
        const Atom clients=XInternAtom(display,"_NET_CLIENT_LIST",False);
        const unsigned long source_pid=getpid();
        const Window source=XCreateSimpleWindow(display,root,20,20,200,200,0,0,0);
        XChangeProperty(display,source,pid_atom,XA_CARDINAL,32,PropModeReplace,reinterpret_cast<const unsigned char *>(&source_pid),1);
        XSelectInput(display,source,ButtonPressMask|ButtonReleaseMask|PointerMotionMask|KeyPressMask|KeyReleaseMask);
        XMapWindow(display,source);XSync(display,False);
        int ready[2];CrossRequire(pipe(ready)==0,"ready pipe failed");ChildOwner destination_owner;destination_owner.pid=fork();
        CrossRequire(destination_owner.pid>=0,"destination fork failed");
        if(destination_owner.pid==0) {
            close(ConnectionNumber(display));close(ready[0]);Display *own=XOpenDisplay(nullptr);if(!own)_exit(2);
            const unsigned long pid=getpid();
            const Window windows[]={XCreateSimpleWindow(own,DefaultRootWindow(own),500,20,200,200,0,0,0),
                                    XCreateSimpleWindow(own,DefaultRootWindow(own),15,15,210,210,0,0,0)};
            XChangeProperty(own,windows[0],pid_atom,XA_CARDINAL,32,PropModeReplace,reinterpret_cast<const unsigned char *>(&pid),1);
            XMapWindow(own,windows[0]);XSync(own,False);
            if(write(ready[1],windows,sizeof(windows))!=sizeof(windows))_exit(3);
            close(ready[1]);for(;;)pause();
        }
        close(ready[1]);Window destination_windows[2]{};
        CrossRequire(read(ready[0],destination_windows,sizeof(destination_windows))==sizeof(destination_windows),"destination window missing");close(ready[0]);
        const Window destination=destination_windows[0],trusted_frame=destination_windows[1];
        const Window fake_frame=XCreateSimpleWindow(display,root,15,15,210,210,0,0,0);
        const std::string nonce="0123456789abcdef0123456789abcdef";
        for(const std::string mode:{"success","framed","fake_frame","override","foreign","desktop_forged","wrong_pid","hover_overlay","cancel","deadline","geometry","mid_overlay","preheld","late_preheld","late_button"}) {
            XReparentWindow(display,source,root,20,20);XMoveWindow(display,destination,500,20);
            XUnmapWindow(display,trusted_frame);XUnmapWindow(display,fake_frame);
            XDeleteProperty(display,root,check);XDeleteProperty(display,root,clients);
            Window obstacle=None;
            if(mode=="framed"||mode=="fake_frame"||mode=="override") {
                const Window frame=mode=="fake_frame"?fake_frame:trusted_frame;
                XChangeProperty(display,root,check,XA_WINDOW,32,PropModeReplace,reinterpret_cast<const unsigned char *>(&destination),1);
                XChangeProperty(display,destination,check,XA_WINDOW,32,PropModeReplace,reinterpret_cast<const unsigned char *>(&destination),1);
                const Window listed[]={source,destination};
                XChangeProperty(display,root,clients,XA_WINDOW,32,PropModeReplace,reinterpret_cast<const unsigned char *>(listed),2);
                XReparentWindow(display,source,frame,5,5);XMapWindow(display,frame);
            }
            XMapRaised(display,source);XRaiseWindow(display,destination);XSetInputFocus(display,source,RevertToParent,CurrentTime);
            if(mode=="override"||mode=="foreign"||mode=="desktop_forged") {
                obstacle=XCreateSimpleWindow(display,root,mode=="override"?220:333,20,mode=="foreign"?1:5,200,0,0,0);
                if(mode=="override") {XSetWindowAttributes a{};a.override_redirect=True;XChangeWindowAttributes(display,obstacle,CWOverrideRedirect,&a);}
                if(mode=="desktop_forged") {
                    const Atom type=XInternAtom(display,"_NET_WM_WINDOW_TYPE",False),desktop=XInternAtom(display,"_NET_WM_WINDOW_TYPE_DESKTOP",False);
                    XChangeProperty(display,obstacle,type,XA_ATOM,32,PropModeReplace,reinterpret_cast<const unsigned char *>(&desktop),1);
                    XChangeProperty(display,obstacle,pid_atom,XA_CARDINAL,32,PropModeReplace,reinterpret_cast<const unsigned char *>(&source_pid),1);
                }
                XMapRaised(display,obstacle);
            }
            XTestFakeMotionEvent(display,-1,70,90,0);XSync(display,False);
            while(XPending(display)){XEvent event{};XNextEvent(display,&event);}
            if(mode=="preheld"){XTestFakeButtonEvent(display,1,True,0);XSync(display,False);}
            const auto until=std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now().time_since_epoch()).count()
                +(mode=="deadline"?300000000LL:4000000000LL);
            std::vector<std::string> args={"helper","drag-between","--source-pid",std::to_string(source_pid),
                "--destination-pid",std::to_string(mode=="wrong_pid"?2147483647L:destination_owner.pid),
                "--source-x","70","--source-y","90","--destination-x","570","--destination-y","90",
                "--source-window-id",std::to_string(source),"--source-window-x","20","--source-window-y","20","--source-window-width","200","--source-window-height","200",
                "--destination-window-id",std::to_string(destination),"--destination-window-x","500","--destination-window-y","20","--destination-window-width","200","--destination-window-height","200",
                "--button","1","--modifiers","ctrl","--path-token",nonce,"--deadline-monotonic-ns",std::to_string(until)};
            int input[2],output[2];CrossRequire(pipe(input)==0&&pipe(output)==0,"helper pipes failed");
            const pid_t worker=fork();CrossRequire(worker>=0,"helper fork failed");
            if(worker==0){close(ConnectionNumber(display));close(input[1]);dup2(input[0],0);close(input[0]);close(output[0]);dup2(output[1],1);close(output[1]);
                std::vector<char*> raw;for(auto &arg:args)raw.push_back(arg.data());_exit(aad_xtest_helper_main(static_cast<int>(raw.size()),raw.data()));}
            close(input[0]);close(output[1]);fcntl(output[0],F_SETFL,fcntl(output[0],F_GETFL)|O_NONBLOCK);
            unsigned down=0,up=0,keys_down=0,keys_up=0,validations=0;std::vector<std::pair<int,int>>trace;
            std::string transcript;std::size_t cursor=0;int status=0;
            auto events=[&]{XSync(display,False);while(XPending(display)){XEvent e{};XNextEvent(display,&e);
                if(e.type==ButtonPress)++down;
                if(e.type==ButtonRelease)++up;
                if(e.type==KeyPress)++keys_down;
                if(e.type==KeyRelease)++keys_up;
                if(e.type==MotionNotify&&(e.xmotion.state&Button1Mask))trace.emplace_back(e.xmotion.x_root,e.xmotion.y_root);}};
            const auto started=std::chrono::steady_clock::now();
            for(;;){events();char buffer[4096];ssize_t count;while((count=read(output[0],buffer,sizeof(buffer)))>0)transcript.append(buffer,static_cast<std::size_t>(count));
                for(;;){const auto end=transcript.find('\n',cursor);if(end==std::string::npos)break;const auto line=transcript.substr(cursor,end-cursor);cursor=end+1;
                    if(line.find("validate_segment")==std::string::npos)continue;
                    const auto index=std::stoul(line.substr(line.find("\"index\":")+8));CrossRequire(index==validations++,"wrong validation sequence");
                    if(index==1&&mode=="hover_overlay"){obstacle=XCreateSimpleWindow(display,root,40,40,80,80,0,0,0);XMapRaised(display,obstacle);XSync(display,False);continue;}
                    if((index==1&&mode=="late_preheld")||(index==2&&mode=="late_button")){XTestFakeButtonEvent(display,1,True,0);XSync(display,False);}
                    if(index==3&&mode=="cancel"){kill(worker,SIGTERM);continue;}
                    if(index==3&&mode=="deadline")continue;
                    if(index==3&&mode=="geometry"){XMoveWindow(display,destination,510,20);XSync(display,False);continue;}
                    if(index==6&&mode=="mid_overlay"){obstacle=XCreateSimpleWindow(display,root,250,20,4,200,0,0,0);XMapRaised(display,obstacle);XSync(display,False);}
                    const auto ack="ack "+nonce+" "+std::to_string(index)+"\n";
                    CrossRequire(write(input[1],ack.data(),ack.size())==static_cast<ssize_t>(ack.size()),"ACK failed");}
                if(waitpid(worker,&status,WNOHANG)==worker)break;
                if(std::chrono::steady_clock::now()-started>std::chrono::seconds(5)){kill(worker,SIGKILL);waitpid(worker,&status,0);throw std::runtime_error("helper cleanup timed out");}usleep(1000);}
            events();close(input[1]);close(output[0]);if(obstacle!=None)XDestroyWindow(display,obstacle);
            CrossRequire(WIFEXITED(status),"helper did not stop cooperatively");
            if(mode=="success"||mode=="framed"){
                if(WEXITSTATUS(status)!=0)std::cerr<<transcript<<'\n';
                CrossRequire(WEXITSTATUS(status)==0&&down==1&&up==1&&trace.size()==12&&validations==16,"cross drag event counts differ");
                for(int i=1;i<=12;++i)CrossRequire(trace[static_cast<std::size_t>(i-1)]==std::pair<int,int>{70+500*i/12,90},"actual cross route differs");
            }else if(mode=="preheld"||mode=="late_preheld"||mode=="late_button"){
                CrossRequire(up==0&&keys_down==(mode=="late_button"?1U:0U)&&validations==(mode=="preheld"?0U:mode=="late_preheld"?2U:3U),"external held input was released");XTestFakeButtonEvent(display,1,False,0);XSync(display,False);
            }else if(mode=="cancel"||mode=="deadline"||mode=="geometry"||mode=="mid_overlay"){
                CrossRequire(down==1&&up==1&&trace.size()<12,"interrupted cross route left held input or continued");
            }else CrossRequire(down==0&&keys_down==0&&trace.empty(),"untrusted route dispatched input");
            CrossRequire(keys_down==keys_up,"modifier was not released");char held[32]{};XQueryKeymap(display,held);for(char key:held)CrossRequire(key==0,"modifier remains held");
            Window r=None,c=None;int rx=0,ry=0,wx=0,wy=0;unsigned mask=0;CrossRequire(XQueryPointer(display,root,&r,&c,&rx,&ry,&wx,&wy,&mask)&&!(mask&Button1Mask),"button remains held");
            std::cout<<"PASS "<<mode<<" down="<<down<<" up="<<up<<" moves="<<trace.size()<<" validations="<<validations<<'\n';
        }
        XCloseDisplay(display);return 0;
    }catch(const std::exception &error){std::cerr<<"FAIL: "<<error.what()<<'\n';return 1;}
}
