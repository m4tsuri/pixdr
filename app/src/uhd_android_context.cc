// Android USB context adapter exported by libpixdr and discovered by libuhd.

#include "pixdr/src/android_uhd_context.rs.h"
#include <uhd/transport/android_usb_context.hpp>
#include <string>

namespace {

class PixdrAndroidUsbContext final : public uhd::transport::android_usb_context
{
public:
    intptr_t fd() const override
    {
        return static_cast<intptr_t>(pixdr::android_usb_fd());
    }

    std::string usbfs_path() const override
    {
        rust::String path = pixdr::android_usbfs_path();
        return std::string(path.data(), path.size());
    }

    uint16_t vid() const override
    {
        return pixdr::android_usb_vid();
    }

    uint16_t pid() const override
    {
        return pixdr::android_usb_pid();
    }

    bool firmware_loaded() const override
    {
        return pixdr::android_usb_firmware_loaded();
    }
};

} // namespace

extern "C"
const uhd::transport::android_usb_context* pixdr_make_uhd_android_usb_context()
{
    static PixdrAndroidUsbContext context;
    return &context;
}
