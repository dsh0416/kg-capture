use std::process::exit;

use windows::{
    core::{BOOL, PCSTR},
    Win32::{Foundation::{HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM}, Graphics::{Direct3D::*, Direct3D11::*, Dxgi::*, Dxgi::Common::*, Gdi::*}, Storage::Xps::*, System::LibraryLoader::GetModuleHandleW, UI::WindowsAndMessaging::*},
};

fn main() {
    // Get capture window rect once to get the size
    let mut rect: RECT = RECT::default();
    let hwnd_capture = find_window();
    _ = unsafe { GetWindowRect(hwnd_capture, &mut rect) };
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    println!("Window found: {:?}", hwnd_capture);
    println!("Window size: {}x{}", width, height);

    let hwnd = init_window(width, height);

    let (device, device_context, swapchain) = init_d3d11(hwnd, width, height);

    loop {
        // Handle messages using PeekMessageA
        let mut msg = MSG::default();
        while unsafe { PeekMessageA(&mut msg, Some(hwnd), 0, 0, PM_REMOVE) }.0 != 0 {
            unsafe {
                _ = TranslateMessage(&msg);
                DispatchMessageA(&msg);
            }
        }

        // Render the frame
        unsafe {
            // Get the swapchain buffer
            let frame_buffer = swapchain.GetBuffer::<ID3D11Texture2D>(0).expect("Failed to get buffer");    
            // Create a render target view
            let mut rtv: Option<ID3D11RenderTargetView> = None;
            device.CreateRenderTargetView(&frame_buffer, None, Some(&mut rtv)).expect("Failed to create render target view");
            device_context.OMSetRenderTargets(Some(&[rtv.clone()]), None);
            
            // clear
            let clear_color = [0.0, 1.0, 0.0, 1.0];
            device_context.ClearRenderTargetView(rtv.as_ref().unwrap(), &clear_color);

            // copy
            let texture = capture_image(hwnd_capture, width, height, device.clone());
            device_context.CopyResource(&frame_buffer, &texture);

            // Present the frame
            let res = swapchain.Present(1, DXGI_PRESENT(0));
            if res.is_err() {
                println!("Failed to present frame: {:?}", res);
            }
        }
    }
}

fn init_window(width: i32, height: i32) -> HWND {
    let wc = WNDCLASSEXA {
        cbSize: std::mem::size_of::<WNDCLASSEXA>() as u32,
        style: CS_CLASSDC,
        lpfnWndProc: Some(wnd_proc),
        hInstance: unsafe { GetModuleHandleW(None).unwrap().into() },
        lpszClassName: PCSTR("KG Capture\0".as_ptr()),
        ..Default::default()
    };

    unsafe { RegisterClassExA(&wc) };

    let caption_height = unsafe { GetSystemMetrics(SM_CYFRAME) + GetSystemMetrics(SM_CYCAPTION) + GetSystemMetrics(SM_CXPADDEDBORDER) };

    let hwnd = unsafe { CreateWindowExA(
        WINDOW_EX_STYLE(0),
        wc.lpszClassName,
        PCSTR("KG Capture\0".as_ptr()),
        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
        100,
        100,
        width,
        height + caption_height,
        None,
        None,
        Some(wc.hInstance),
        None
    ) }.unwrap();

    unsafe { _ = ShowWindow(hwnd, SW_SHOW); };
    unsafe { _ = UpdateWindow(hwnd) };

    hwnd
}

fn init_d3d11(hwnd: HWND, width: i32, height: i32) -> (ID3D11Device, ID3D11DeviceContext, IDXGISwapChain1) {
    let mut d3d11_device: Option<ID3D11Device> = None;
    let mut d3d11_device_context: Option<ID3D11DeviceContext> = None;
    
    unsafe { D3D11CreateDevice(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE(std::ptr::null_mut()),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_DEBUG,
        Some(&[D3D_FEATURE_LEVEL_11_0]),
        D3D11_SDK_VERSION,
        Some(&mut d3d11_device),
        None,
        Some(&mut d3d11_device_context)
    ) }.expect("Failed to create D3D11 device");

    let dxgi_factory : IDXGIFactory2 = unsafe { CreateDXGIFactory2::<IDXGIFactory2>(
        DXGI_CREATE_FACTORY_DEBUG
    ) }.expect("Failed to create DXGI factory");

    let swap_chain_desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: width as u32,
        Height: height as u32,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        ..Default::default()
    };

    let swap_chain: IDXGISwapChain1 = unsafe { dxgi_factory.CreateSwapChainForHwnd(
        d3d11_device.as_ref().unwrap(),
        hwnd,
        &swap_chain_desc,
        None,
        None
    ) }.expect("Failed to create swap chain");

    (d3d11_device.unwrap(), d3d11_device_context.unwrap(), swap_chain)
}

fn capture_image(hwnd: HWND, width: i32, height: i32, device: ID3D11Device) -> ID3D11Texture2D{
    let window_dc = unsafe { GetDC(Some(hwnd)) };
    let capture_dc = unsafe { CreateCompatibleDC(Some(window_dc)) };

    let bitmap = unsafe { CreateCompatibleBitmap(window_dc, width, height) };
    _ = unsafe { SelectObject(capture_dc, bitmap.into()) };
    _ = unsafe { UpdateWindow(hwnd) };

    let mut found = false;

    while !found {
        // 全民K歌 set WS_EX_LAYERED, which causes the GDI or DXGI to not capture the window correctly
        // Fortunately, PrintWindow with the PRINT_WINDOW_FLAGS(2) flag works
        _ = unsafe { PrintWindow(hwnd, capture_dc, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT))};

        found = true;
    
        // 全民K歌 would try to cover the window with white after receiving WM_PAINT message, considering vertical sync, white may not fill every row.
        // Here we only check the first pixel of each row, if any of them is white, it means the window is covered, ignore this bitmap
        // otherwise, break the loop
        for i in 0..height {
            let pixel_row = unsafe { GetPixel(capture_dc, 0, i) }.0;
            let r = (pixel_row & 0xFF) as u8;
            if r == 0xFF {
                found = false;
                break;
            }
        }
    }
    
    // create a d3d11 texture from the bitmap
    let texture_desc = D3D11_TEXTURE2D_DESC {
        Width: width as u32,
        Height: height as u32,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };

    let mut buffer_data = vec![0u8; (width * height * 4) as usize];

    // create bitmap from the HBITMAP
    let bitmap_header = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        biHeight: -height, // Negative to indicate a top-down bitmap
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        biSizeImage: 0,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
        ..Default::default()
    };

    let mut bitmap_info = BITMAPINFO {
        bmiHeader: bitmap_header,
        ..Default::default()
    };

    let result = unsafe {
        GetDIBits(
            capture_dc,
            bitmap,
            0,
            height as u32,
            Some(buffer_data.as_mut_ptr() as *mut _),
            &mut bitmap_info as *mut _ as *mut BITMAPINFO,
            DIB_RGB_COLORS
        )
    };
    if result == 0 {
        panic!("Failed to get DIB bits");
    }

    let texture_data = D3D11_SUBRESOURCE_DATA {
        pSysMem: buffer_data.as_ptr() as *const _,
        SysMemPitch: (width * 4) as u32,
        SysMemSlicePitch: (width * height * 4) as u32,
    };

    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&texture_desc, Some(&texture_data), Some(&mut texture)) }.expect("Failed to create texture");

    // free resources
    unsafe {
        _ = DeleteObject(bitmap.into());
        _ = DeleteDC(capture_dc);
        ReleaseDC(Some(hwnd), window_dc);
    }

    texture.unwrap()
}

fn find_window() -> HWND {
    let mut hwnd_capture: Option<HWND> = None;
    _ = unsafe { EnumWindows(Some(enum_windows_callback), LPARAM((&mut hwnd_capture) as *const _ as isize)) };
    if hwnd_capture.is_none() { panic!("Window not found"); }

    hwnd_capture.unwrap()
}

unsafe extern "system" fn enum_windows_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let mut class_name = [0u16; 256];
    let mut title = [0u16; 256];

    unsafe {
        GetClassNameW(hwnd, &mut class_name);
        GetWindowTextW(hwnd, &mut title);
        
        if !class_name.is_empty() && !title.is_empty() {
            let str_title = String::from_utf16_lossy(&title);
           
            if str_title.contains("CLyricRenderWnd") {
                let hwnd_capture = &mut *(lparam.0 as *mut Option<HWND>);
                *hwnd_capture = Some(hwnd);

                return BOOL(0);
            }
        }
    }

    return BOOL(1); // Continue enumeration
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_DESTROY => {
            // Handle window destruction
            println!("Window destroyed");
            unsafe { PostQuitMessage(0) };
            exit(0);
        }
        _ => {
            return unsafe { DefWindowProcA(hwnd, msg, wparam, lparam) };
        }
    }
}
