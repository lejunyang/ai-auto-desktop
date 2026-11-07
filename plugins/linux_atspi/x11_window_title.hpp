// Optional, read-only ICCCM title decoding for coordinate identity. Kept in
// this bounded helper process so an Xlib connection failure cannot terminate
// the Rust control server. Partial or lossy conversions never establish identity.
std::string WindowTitleJson(Display *display, Window window) {
    Atom type=None; int format=0; unsigned long count=0, remaining=0;
    unsigned char *value=nullptr;
    g_x_error=false;
    const int result=XGetWindowProperty(display,window,XA_WM_NAME,0,1024,False,AnyPropertyType,
        &type,&format,&count,&remaining,&value);
    if (result!=Success || g_x_error || format!=8 || remaining || !value || count>4096
        || std::memchr(value,0,count)!=nullptr) {
        if (value) XFree(value);
        return {};
    }
    const Atom compound=XInternAtom(display,"COMPOUND_TEXT",False);
    if (type!=compound || !std::setlocale(LC_CTYPE,"") || !XSupportsLocale()) {
        XFree(value); return {};
    }
    XTextProperty property{value,type,format,count};
    char **list=nullptr; int strings=0;
    const int converted=Xutf8TextPropertyToTextList(display,&property,&list,&strings);
    std::string output;
    if (converted==Success && strings==1 && list && list[0]) {
        const std::size_t length=strnlen(list[0],16385);
        if (length<=16384) {
            const std::string title(list[0],length);
            if (!title.empty() && DecodeUtf8(title).has_value()) {
                static constexpr char hex[]="0123456789abcdef";
                std::string raw; raw.reserve(count*2);
                for(unsigned long i=0;i<count;++i) {
                    raw+=hex[value[i]>>4];raw+=hex[value[i]&15];
                }
                output=",\"wm_name_utf8\":"+JsonString(title)+",\"wm_name_raw_hex\":"+JsonString(raw);
            }
        }
    }
    if (list) XFreeStringList(list);
    XFree(value);
    return output;
}
