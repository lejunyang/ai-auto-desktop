// A private X server; known wire bytes, rather than the encoder under test.
#define main aad_xtest_helper_main
#include "../../plugins/linux_atspi/x11_xtest_helper.cpp"
#undef main
#include <stdexcept>

int main() {
    Display *display=XOpenDisplay(nullptr);
    if(!display) return 1;
    XSetErrorHandler(HandleXError);
    const Window window=XCreateSimpleWindow(display,DefaultRootWindow(display),0,0,100,100,0,0,0);
    const Atom compound=XInternAtom(display,"COMPOUND_TEXT",False);
    auto property=[&](Atom type,const std::string &bytes,int format=8) {
        XChangeProperty(display,window,XA_WM_NAME,type,format,PropModeReplace,
            reinterpret_cast<const unsigned char *>(bytes.data()),static_cast<int>(bytes.size()/(format/8)));
        XSync(display,False); return WindowTitleJson(display,window);
    };
    int passed=0;
    auto require=[&](bool ok,const char *name) {
        if(!ok) throw std::runtime_error(name);
        ++passed;std::cout<<"PASS: "<<name<<std::endl;
    };
    try {
        const std::string wire="\x1b$(BCfJ8\x1b(B-folder";
        const auto decoded=property(compound,wire);
        require(decoded.find("\"wm_name_utf8\":"+JsonString("中文-folder"))!=std::string::npos,
            "known compound text has exact Unicode identity");
        require(decoded.find("1b24284243664a381b28422d666f6c646572")!=std::string::npos,
            "conversion returns complete original property bytes");
        require(property(compound,std::string("one\0two",7)).empty(),"multiple strings refused");
        require(property(compound,std::string(4097,'a')).empty(),"truncated oversized property refused");
        require(property(compound,std::string(4,'a'),16).empty(),"wrong property format refused");
        require(property(XA_STRING,wire).empty(),"wrong property encoding refused");
        require(property(compound,"").empty(),"empty conversion refused");
        require(property(compound,"\x1b%G\xff\x1b%@").empty(),"invalid compound UTF-8 refused");
    } catch(const std::exception &e) {
        std::cerr<<"FAIL: "<<e.what()<<std::endl;XDestroyWindow(display,window);XCloseDisplay(display);return 2;
    }
    XDestroyWindow(display,window);XCloseDisplay(display);
    return passed==8?0:3;
}
