use std::{cmp::max, process::exit};

use windows::{
    core::{BOOL, PCSTR},
    Win32::{
        Foundation::{HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::{Direct3D::*, Direct3D11::*, Dxgi::{Common::*, *}, Gdi::*},
        Storage::Xps::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::*
    },
};

fn main() {
    // Get capture window rect once to get the size
    let mut rect_lyrics: RECT = RECT::default();
    let hwnd_capture_lyrics = find_lyrics_window();
    _ = unsafe { GetWindowRect(hwnd_capture_lyrics, &mut rect_lyrics) };
    let width_lyrics = rect_lyrics.right - rect_lyrics.left;
    let height_lyrics = rect_lyrics.bottom - rect_lyrics.top;
    println!("Lyrics Window found: {:?}", hwnd_capture_lyrics);
    println!("Lyrics Window size: {}x{}", width_lyrics, height_lyrics);

    // Get score window rect once to get the size
    let mut rect_score: RECT = RECT::default();
    let hwnd_capture_score = find_score_window();
    _ = unsafe { GetWindowRect(hwnd_capture_score, &mut rect_score) };
    let width_score = rect_score.right - rect_score.left;
    let height_score = rect_score.bottom - rect_score.top;
    println!("Score Window found: {:?}", hwnd_capture_score);
    println!("Score Window size: {}x{}", width_score, height_score);

    let hwnd = init_window(width_lyrics, height_lyrics, width_score, height_score);

    let (device, device_context, swapchain) = init_d3d11(hwnd, max(width_lyrics, width_score), height_lyrics + height_score);

    let frame_buffer = unsafe { swapchain.GetBuffer::<ID3D11Texture2D>(0).expect("Failed to get buffer") };

    let mut rtv: Option<ID3D11RenderTargetView> = None;

    // Create a render target view
    unsafe { device.CreateRenderTargetView(&frame_buffer, None, Some(&mut rtv)).expect("Failed to create render target view") };
    unsafe { device_context.OMSetRenderTargets(Some(&[rtv.clone()]), None) };

    // clear
    let clear_color = [0.0, 0.0, 0.0, 1.0];
    unsafe { device_context.ClearRenderTargetView(rtv.as_ref().unwrap(), &clear_color) };

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
            // copy
            let texture_score = capture_image_score(hwnd_capture_score, width_score, height_score, device.clone());
            let texture_lyrics = capture_image_lyrics(hwnd_capture_lyrics, width_lyrics, height_lyrics, device.clone());
            if texture_lyrics.is_some() {
                device_context.CopySubresourceRegion(&frame_buffer, 0, 0, 0, 0,  &texture_lyrics.unwrap(), 0, None);
            }
            device_context.CopySubresourceRegion(&frame_buffer, 0, 0, height_lyrics.try_into().unwrap(), 0, &texture_score, 0, None);

            // Present the frame
            let res = swapchain.Present(1, DXGI_PRESENT(0));
            if res.is_err() {
                println!("Failed to present frame: {:?}", res);
            }
        }
    }
}

fn init_window(lyrics_width: i32, lyrics_height: i32, score_width: i32, score_height: i32) -> HWND {
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
    let border_thickness_x = unsafe { GetSystemMetrics(SM_CXFRAME) };
    let border_thickness_y = unsafe { GetSystemMetrics(SM_CYFRAME ) };
    let padding_x = unsafe { GetSystemMetrics(SM_CXPADDEDBORDER) };
    let window_width = max(lyrics_width, score_width) + border_thickness_x * 2 + padding_x;
    let window_height = lyrics_height + score_height + border_thickness_y * 2 + caption_height;

    let hwnd = unsafe { CreateWindowExA(
        WINDOW_EX_STYLE(0),
        wc.lpszClassName,
        PCSTR("KG Capture\0".as_ptr()),
        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
        100,
        100,
        window_width,
        window_height,
        None,
        None,
        Some(wc.hInstance),
        None
    ) }.unwrap();

    unsafe { _ = ShowWindow(hwnd, SW_SHOW); };
    unsafe { _ = UpdateWindow(hwnd) };

    hwnd
}

fn init_d3d11(hwnd: HWND, width: i32, height: i32) -> (ID3D11Device, ID3D11DeviceContext, IDXGISwapChain) {
    let swap_chain_desc = DXGI_SWAP_CHAIN_DESC {
        BufferDesc: DXGI_MODE_DESC {
            Width: width as u32,
            Height: height as u32,
            RefreshRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            ..Default::default()
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        OutputWindow: hwnd,
        Windowed: BOOL(1),
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
        ..Default::default()
    };

    let mut d3d11_device: Option<ID3D11Device> = None;
    let mut d3d11_device_context: Option<ID3D11DeviceContext> = None;
    let mut swap_chain: Option<IDXGISwapChain> = None;

    let result = unsafe { D3D11CreateDeviceAndSwapChain(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE(std::ptr::null_mut()),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        None,
        D3D11_SDK_VERSION,
        Some(&swap_chain_desc),
        Some(&mut swap_chain),
        Some(&mut d3d11_device),
        None,
        Some(&mut d3d11_device_context)
    ) };

    if result.is_err() {
        // Fallback to WARP driver
        unsafe { D3D11CreateDeviceAndSwapChain(
            None,
            D3D_DRIVER_TYPE_WARP,
            HMODULE(std::ptr::null_mut()),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&swap_chain_desc),
            Some(&mut swap_chain),
            Some(&mut d3d11_device),
            None,
            Some(&mut d3d11_device_context)
        ) }.expect("Failed to create D3D11 device and swap chain");
    }

    (d3d11_device.unwrap(), d3d11_device_context.unwrap(), swap_chain.unwrap())
}

fn capture_image_lyrics(hwnd: HWND, lyrics_width: i32, lyrics_height: i32, device: ID3D11Device) -> Option<ID3D11Texture2D>{
    let window_dc = unsafe { GetDC(Some(hwnd)) };
    let capture_dc = unsafe { CreateCompatibleDC(Some(window_dc)) };

    let bitmap = unsafe { CreateCompatibleBitmap(window_dc, lyrics_width, lyrics_height) };
    _ = unsafe { SelectObject(capture_dc, bitmap.into()) };

    // 全民K歌 set WS_EX_LAYERED, which causes the GDI or DXGI to not capture the window correctly
    // Fortunately, PrintWindow with the PRINT_WINDOW_FLAGS(2) flag works
    _ = unsafe { PrintWindow(hwnd, capture_dc, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT))}; 

    let mut found = true;

    // try at most 100 times to get the bitmap
    for _ in 1..100 {
        // 全民K歌 would try to cover the window with white after receiving WM_PAINT message, considering vertical sync, white may not fill every row.
        // Here we only check the first pixel of each row, if any of them is white, it means the window is covered, ignore this bitmap
        // otherwise, break the loop
        for i in 0..lyrics_height {
            let pixel_row = unsafe { GetPixel(capture_dc, 0, i) }.0;
            let r = (pixel_row & 0xFF) as u8;
            if r == 0xFF {
                found = false;
                break;
            }
        }

        if found { break; }
    }

    if !found {
        // flickering, return None
        return None;
    }

    // create a d3d11 texture from the bitmap
    let texture_desc = D3D11_TEXTURE2D_DESC {
        Width: lyrics_width as u32,
        Height: lyrics_height as u32,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };

    let mut buffer_data = vec![0u8; (lyrics_width * lyrics_height * 4) as usize];

    // create bitmap from the HBITMAP
    let bitmap_header = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: lyrics_width,
        biHeight: -lyrics_height, // Negative to indicate a top-down bitmap
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
            lyrics_height as u32,
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
        SysMemPitch: (lyrics_width * 4) as u32,
        SysMemSlicePitch: (lyrics_width * lyrics_height * 4) as u32,
    };

    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&texture_desc, Some(&texture_data), Some(&mut texture)) }.expect("Failed to create texture");

    // free resources
    unsafe {
        _ = DeleteObject(bitmap.into());
        _ = DeleteDC(capture_dc);
        ReleaseDC(Some(hwnd), window_dc);
    }

    Some(texture.unwrap())
}

fn capture_image_score(hwnd: HWND, score_width: i32, score_height: i32, device: ID3D11Device) -> ID3D11Texture2D{
    let window_dc = unsafe { GetDC(Some(hwnd)) };
    let capture_dc = unsafe { CreateCompatibleDC(Some(window_dc)) };

    let bitmap = unsafe { CreateCompatibleBitmap(window_dc, score_width, score_height) };
    _ = unsafe { SelectObject(capture_dc, bitmap.into()) };
    let mut found = false;

    while !found {
        _ = unsafe { PrintWindow(hwnd, capture_dc, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT))};

        found = true;
    }
    
    // create a d3d11 texture from the bitmap
    let texture_desc = D3D11_TEXTURE2D_DESC {
        Width: score_width as u32,
        Height: score_height as u32,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };

    let mut buffer_data = vec![0u8; (score_width * score_height * 4) as usize];

    // create bitmap from the HBITMAP
    let bitmap_header = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: score_width,
        biHeight: -score_height, // Negative to indicate a top-down bitmap
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
            score_height as u32,
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
        SysMemPitch: (score_width * 4) as u32,
        SysMemSlicePitch: (score_width * score_height * 4) as u32,
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

fn find_lyrics_window() -> HWND {
    let mut hwnd_capture: Option<HWND> = None;
    _ = unsafe { EnumWindows(Some(enum_windows_callback_lyrics), LPARAM((&mut hwnd_capture) as *const _ as isize)) };
    if hwnd_capture.is_none() { panic!("Window not found"); }

    hwnd_capture.unwrap()
}

fn find_score_window() -> HWND {
    let mut hwnd_capture: Option<HWND> = None;
    _ = unsafe { EnumWindows(Some(enum_windows_callback_score), LPARAM((&mut hwnd_capture) as *const _ as isize)) };
    if hwnd_capture.is_none() { panic!("Window not found"); }

    hwnd_capture.unwrap()
}


unsafe extern "system" fn enum_windows_callback_lyrics(hwnd: HWND, lparam: LPARAM) -> BOOL {
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

unsafe extern "system" fn enum_windows_callback_score(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let mut class_name = [0u16; 256];
    let mut title = [0u16; 256];

    unsafe {
        GetClassNameW(hwnd, &mut class_name);
        GetWindowTextW(hwnd, &mut title);
        
        if !class_name.is_empty() && !title.is_empty() {
            let str_title = String::from_utf16_lossy(&title);
           
            if str_title.contains("CScoreWnd") {
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
