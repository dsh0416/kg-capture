use std::{cmp::max, ffi::c_void, process::exit, sync::Mutex};

use windows::{
    core::{BOOL, PCSTR},
    Win32::{
        Foundation::{HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::{Direct3D::*, Direct3D11::*, Dxgi::{Common::*, *}, Gdi::*},
        Storage::Xps::*,
        System::LibraryLoader::GetModuleHandleA,
        UI::WindowsAndMessaging::*
    },
};

static RTV: Mutex<Option<ID3D11RenderTargetView>> = Mutex::new(None);

fn main() {
    // Get capture window rect once to get thea size
    let hwnd_capture_lyrics = find_lyrics_window();
    let (mut width_lyrics, mut height_lyrics) = get_window_size(hwnd_capture_lyrics);
    println!("Lyrics Window found: {:?}, size: {}x{}", hwnd_capture_lyrics, width_lyrics, height_lyrics);

    // Get score window rect once to get the size
    let hwnd_capture_score = find_score_window();
    let (mut width_score, mut height_score) = get_window_size(hwnd_capture_score);
    println!("Score Window found: {:?}, size: {}x{}", hwnd_capture_score, width_score, height_score);

    let (width, height) = calc_window_size(width_lyrics, height_lyrics, width_score, height_score);
    let hwnd = init_window(width, height);

    let (device, device_context, swapchain) = init_d3d11(hwnd);

    // Clear the RTV
    {
        let frame_buffer = unsafe { swapchain.GetBuffer::<ID3D11Texture2D>(0).expect("Failed to get buffer") };
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        unsafe { device.CreateRenderTargetView(&frame_buffer, None, Some(&mut rtv)).expect("Failed to create render target view") };
        unsafe { device_context.OMSetRenderTargets(Some(&[rtv.clone()]), None) };
        RTV.lock().unwrap().replace(rtv.clone().unwrap());
    }

    loop {
        // Handle messages using PeekMessageA
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageA(&mut msg, Some(hwnd), 0, 0, PM_REMOVE).0 != 0 {
                _ = TranslateMessage(&msg);
                DispatchMessageA(&msg);
            }
        }

        // Check if the window is resized
        {
            let (new_width_lyrics, new_height_lyrics) = get_window_size(hwnd_capture_lyrics);
            let (new_width_score, new_height_score) = get_window_size(hwnd_capture_score);
            if new_width_lyrics != width_lyrics || new_height_lyrics != height_lyrics ||
                new_width_score != width_score || new_height_score != height_score {
                width_lyrics = new_width_lyrics;
                height_lyrics = new_height_lyrics;
                width_score = new_width_score;
                height_score = new_height_score;

                println!("Lyrics Window resized: {}x{}", width_lyrics, height_lyrics);
                println!("Score Window resized: {}x{}", width_score, height_score);

                // Resize the window
                let mut rect: RECT = RECT::default();
                _ = unsafe { GetClientRect(hwnd, &mut rect) };
                let (width, height) = calc_window_size(width_lyrics, height_lyrics, width_score, height_score);
                _ = unsafe { SetWindowPos(hwnd, None, 0, 0, width, height, SWP_NOZORDER | SWP_NOMOVE) };

                // Resize the swap chain
                { *RTV.lock().unwrap() = None; }
                unsafe { device_context.OMSetRenderTargets(None, None) };
                unsafe { swapchain.ResizeBuffers(0, 0, 0, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0)) }.expect("Failed to resize swap chain");
                let frame_buffer = unsafe { swapchain.GetBuffer::<ID3D11Texture2D>(0).expect("Failed to get buffer") };

                let mut rtv: Option<ID3D11RenderTargetView> = None;             
                unsafe { device.CreateRenderTargetView(&frame_buffer, None, Some(&mut rtv)).expect("Failed to create render target view") };
                unsafe { device_context.OMSetRenderTargets(Some(&[rtv.clone()]), None) };
                RTV.lock().unwrap().replace(rtv.clone().unwrap());
            }
        }

        // Render the frame
        {
            let frame_buffer = unsafe { swapchain.GetBuffer::<ID3D11Texture2D>(0).expect("Failed to get buffer") };

            // copya
            let texture_score = capture_image_score(hwnd_capture_score, width_score, height_score, device.clone());
            let texture_lyrics = capture_image_lyrics(hwnd_capture_lyrics, width_lyrics, height_lyrics, device.clone());
            if texture_lyrics.is_some() {
                unsafe { device_context.CopySubresourceRegion(&frame_buffer, 0, 0, 0, 0,  &texture_lyrics.unwrap(), 0, None) };
            }
            unsafe { device_context.CopySubresourceRegion(&frame_buffer, 0, 0, height_lyrics.try_into().unwrap(), 0, &texture_score, 0, None) };

            // Present the frame
            let res = unsafe { swapchain.Present(1, DXGI_PRESENT(0)) };
            if res.is_err() {
                println!("Failed to present frame: {:?}", res);
            }
        }
    }
}

fn calc_window_size(lyrics_width: i32, lyrics_height: i32, score_width: i32, score_height: i32) -> (i32, i32) {
    let caption_height = unsafe { GetSystemMetrics(SM_CYFRAME) + GetSystemMetrics(SM_CYCAPTION) + GetSystemMetrics(SM_CXPADDEDBORDER) };
    let border_thickness_x = unsafe { GetSystemMetrics(SM_CXFRAME) };
    let border_thickness_y = unsafe { GetSystemMetrics(SM_CYFRAME ) };
    let padding_x = unsafe { GetSystemMetrics(SM_CXPADDEDBORDER) };
    let width = max(lyrics_width, score_width) + border_thickness_x * 2 + padding_x;
    let height = lyrics_height + score_height + border_thickness_y * 2 + caption_height;

    (width, height)
}

fn init_window(width: i32, height: i32) -> HWND {
    let wc = WNDCLASSEXA {
        cbSize: std::mem::size_of::<WNDCLASSEXA>() as u32,
        style: CS_CLASSDC,
        lpfnWndProc: Some(wnd_proc),
        hInstance: unsafe { GetModuleHandleA(None).unwrap().into() },
        lpszClassName: PCSTR("KG Capture\0".as_ptr()),
        ..Default::default()
    };

    unsafe { RegisterClassExA(&wc) };

    let hwnd = unsafe { CreateWindowExA(
        WINDOW_EX_STYLE(0),
        wc.lpszClassName,
        PCSTR("KG Capture\0".as_ptr()),
        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
        100,
        100,
        width,
        height,
        None,
        None,
        Some(wc.hInstance),
        None
    ) }.unwrap();

    unsafe { _ = ShowWindow(hwnd, SW_SHOW); };
    unsafe { _ = UpdateWindow(hwnd) };

    hwnd
}

fn init_d3d11(hwnd: HWND) -> (ID3D11Device, ID3D11DeviceContext, IDXGISwapChain) {
    let swap_chain_desc = DXGI_SWAP_CHAIN_DESC {
        BufferDesc: DXGI_MODE_DESC {
            Width: 0,
            Height: 0,
            RefreshRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            ..Default::default()
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 1,
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

fn get_window_size(hwnd: HWND) -> (i32, i32) {
    let mut rect: RECT = RECT::default();
    _ = unsafe { GetClientRect(hwnd, &mut rect) };
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    (width, height)
}

fn capture_image_lyrics(hwnd: HWND, lyrics_width: i32, lyrics_height: i32, device: ID3D11Device) -> Option<ID3D11Texture2D>{
    let window_dc = unsafe { GetDC(Some(hwnd)) };
    let capture_dc = unsafe { CreateCompatibleDC(Some(window_dc)) };

    let bitmap_header = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: lyrics_width,
        biHeight: -lyrics_height, // Negative to indicate a top-down bitmap
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        biSizeImage: (lyrics_width * lyrics_height) as u32,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
        ..Default::default()
    };

    let bitmap_info = BITMAPINFO {
        bmiHeader: bitmap_header,
        ..Default::default()
    };

    // create an empty pointer that will be filled with the bitmap data
    let mut buffer_ptr: *mut c_void = std::ptr::null_mut();
    let bitmap = unsafe { CreateDIBSection(Some(window_dc), &bitmap_info, DIB_RGB_COLORS, &mut buffer_ptr, None, 0) }.unwrap();
    _ = unsafe { SelectObject(capture_dc, bitmap.into()) };

    let mut found = true;

    _ = unsafe { PrintWindow(hwnd, capture_dc, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT))}; 

    for i in 0..lyrics_height {
        let pixel_row = unsafe { GetPixel(capture_dc, 0, i) }.0;
        let r = (pixel_row & 0xFF) as u8;
        if r == 0xFF {
            found = false;
            break;
        }
    }

    if !found {
        _ = unsafe { DeleteObject(bitmap.into()) };
        _ = unsafe { DeleteDC(capture_dc) };
        _ = unsafe { ReleaseDC(Some(hwnd), window_dc) };

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



    // create bitmap from the HBITMAP
    let texture_data = D3D11_SUBRESOURCE_DATA {
        pSysMem: buffer_ptr,
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

