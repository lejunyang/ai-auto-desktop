// No display or input: exercise refreshed-map bounds before indexing a modifier.
#define main aad_xtest_helper_main
#include "../../plugins/linux_atspi/x11_xtest_helper.cpp"
#undef main

int main() {
    XkbDescRec descriptor{};
    XkbClientMapRec map{};
    unsigned char modifiers[31]{};
    descriptor.min_key_code = 20;
    descriptor.max_key_code = 30;
    descriptor.map = &map;
    if (PhysicalShift(nullptr, 20) || PhysicalShift(&descriptor, 20)) return 1;
    map.modmap = modifiers;
    modifiers[20] = ShiftMask;
    modifiers[30] = ShiftMask;
    if (!PhysicalShift(&descriptor, 20) || !PhysicalShift(&descriptor, 30) ||
        PhysicalShift(&descriptor, 19) || PhysicalShift(&descriptor, 31) ||
        PhysicalShift(&descriptor, 255)) return 1;
    descriptor.max_key_code = 19;
    if (PhysicalShift(&descriptor, 20)) return 1;
    std::cout << "PASS: refreshed physical modifier maps reject missing and out-of-range entries\n";
    return 0;
}
