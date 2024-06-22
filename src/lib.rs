use std::io::Cursor;

use futures::future;
use image::{imageops::FilterType, DynamicImage, GenericImageView, ImageError, ImageFormat};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use worker::{
    console_error, console_log, event, send::SendWrapper, Bucket, Context, Env, HttpMetadata,
    Request, Response, Result as WorkerResult, RouteContext, Router,
};

#[macro_use]
mod macros;

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> WorkerResult<Response> {
    console_error_panic_hook::set_once();

    let router = Router::new();
    router
        .get("/", handle_get)
        .post_async("/", handle_post_image)
        .run(req, env)
        .await
}

fn handle_get(_req: Request, _ctx: RouteContext<()>) -> WorkerResult<Response> {
    Response::ok("upix API")
}

// fn get_images(_req: Request, ctx: RouteContext<()>) -> Result<Response> {
//     let bucket = ctx.bucket("IMGS_BUCKET")?;
//     let images = bucket.list().limit(100).execute().await?.objects();
//     console_log!("{}", images.len());
//     if images.is_empty() {
//         return Response::ok("no images found");
//     }

//     let images = images.iter().map(|img| img.key()).collect::<Vec<_>>();
//     Response::from_json(&images)
// }

type SendBucket = SendWrapper<Bucket>;

struct ApiError {
    status: u16,
    message: Option<String>,
}

impl ApiError {
    fn new(status: u16, msg: impl Into<String>) -> Self {
        Self {
            status,
            message: Some(msg.into()),
        }
    }
    fn no_msg(status: u16) -> Self {
        Self {
            status,
            message: None,
        }
    }

    fn to_response(&self) -> WorkerResult<Response> {
        let r = match &self.message {
            None => Response::empty(),
            Some(msg) => Response::from_json(&json!({ "message": msg })),
        };
        r.map(|r| r.with_status(self.status))
    }
}

type ApiResult<T> = std::result::Result<T, ApiError>;

async fn handle_post_image(req: Request, ctx: RouteContext<()>) -> WorkerResult<Response> {
    let res = post_image(req, ctx).await;
    match res {
        Ok(images) => Response::from_json(&images),
        Err(e) => e.to_response(),
    }
}

async fn post_image(mut req: Request, ctx: RouteContext<()>) -> ApiResult<Vec<UploadedImage>> {
    let Ok(bucket) = ctx.bucket("IMGS_BUCKET") else {
        console_error!("failed to get bindings to the R2 bucket");
        return Err(ApiError::no_msg(500));
    };
    let bucket = SendWrapper::new(bucket);

    let Ok(Some(content_type)) = req.headers().get("Content-Type") else {
        return Err(ApiError::new(400, "missing Content-Type header"));
    };
    let img_fmt = validate_img_format(&content_type)?;

    let Ok(body) = req.bytes().await else {
        console_error!("could not read request body from the request");
        return Err(ApiError::no_msg(500));
    };
    // data length limit: 512 KiB
    if body.len() > 512 * 1024 {
        return Err(ApiError::no_msg(413)); // 413 Payload Too Large
    }

    let (img, hash) = load_image_with_hash(body, img_fmt)?;
    validate_img_dimension(&img)?;

    let uploader = ImageUploader {
        img,
        hash,
        dest_fmt: ImageFormat::Png,
        dest_bucket: bucket,
    };

    let tasks: Vec<future::BoxFuture<_>> = map_pin![
        uploader.upload_original_image(),
        uploader.upload_upscaled_image(2),
        uploader.upload_upscaled_image(4),
        uploader.upload_upscaled_image(8),
        uploader.upload_upscaled_image(16),
    ];
    let task_res: Result<Vec<_>, ()> = future::join_all(tasks).await.into_iter().collect();

    task_res.map_err(|e| {
        console_error!("{:?}", e);
        ApiError::new(500, "Internal Server Error")
    })
}

fn validate_img_format(content_type: &str) -> ApiResult<ImageFormat> {
    if !content_type.starts_with("image/") {
        return Err(ApiError::new(400, "Content-Type is not for an image"));
    }
    let Some(img_fmt) = ImageFormat::from_mime_type(content_type) else {
        return Err(ApiError::new(400, "Content-Type is not for an image"));
    };

    match img_fmt {
        ImageFormat::Png | ImageFormat::WebP | ImageFormat::Bmp | ImageFormat::Gif => Ok(img_fmt),
        _ => Err(ApiError::new(
            400,
            format!("unsupported image format: {}", img_fmt.extensions_str()[0]),
        )),
    }
}

const MAX_PIXELS: u32 = 65536;
const MAX_LONG_SIDE_LEN: u32 = 1024;
const MAX_ASPECT_RATIO: f64 = 16.0;

fn validate_img_dimension(img: &DynamicImage) -> ApiResult<()> {
    let (w, h) = img.dimensions();
    if w * h > MAX_PIXELS {
        return Err(ApiError::new(
            400,
            format!("Image has too many pixels ({} > {})", w * h, MAX_PIXELS),
        ));
    }

    let (long, short) = if w > h { (w, h) } else { (h, w) };
    if long > MAX_LONG_SIDE_LEN {
        return Err(ApiError::new(
            400,
            format!(
                "Long side of image is too long ({} > {})",
                long, MAX_LONG_SIDE_LEN
            ),
        ));
    }
    if f64::from(long) / f64::from(short) > MAX_ASPECT_RATIO {
        return Err(ApiError::new(
            400,
            format!(
                "Aspect retio of image is out of range ({} : {} > {} : 1)",
                long, short, MAX_ASPECT_RATIO
            ),
        ));
    }
    Ok(())
}

fn load_image_with_hash(
    img_data: Vec<u8>,
    img_fmt: ImageFormat,
) -> ApiResult<(DynamicImage, String)> {
    let mut hasher = Sha256::new();
    hasher.update(&img_data);
    let hash = hex::encode(hasher.finalize());

    let img = image::load_from_memory_with_format(&img_data, img_fmt).map_err(|e| match e {
        ImageError::Decoding(_) => ApiError::new(400, "failed to decode image"),
        _ => ApiError::no_msg(500),
    })?;

    Ok((img, hash))
}

fn encode_image(img: &DynamicImage, img_fmt: ImageFormat, dest: &mut Vec<u8>) -> Result<(), ()> {
    let mut buf = Cursor::new(dest);
    let write_res = img.write_to(&mut buf, img_fmt);
    match write_res {
        Ok(_) => Ok(()),
        Err(e) => {
            console_error!("failed to write image to buffer: {:?}", e);
            Err(())
        }
    }
}

/// Uploads an image to a bucket. Returns the file name (stem + extension for the image format) of the uploaded image if succeeded.
#[worker::send]
async fn upload_image_to_bucket(
    stem: &str,
    data: Vec<u8>,
    img_fmt: ImageFormat,
    bucket: SendBucket,
) -> Result<String, ()> {
    console_log!("uploading image... (stem: {})", stem);

    let key = format!("{}.{}", stem, img_fmt.extensions_str()[0]);
    let meta = HttpMetadata {
        content_type: Some(img_fmt.to_mime_type().to_string()),
        ..HttpMetadata::default()
    };

    let put_res = bucket.put(&key, data).http_metadata(meta).execute().await;
    match put_res {
        Ok(_) => Ok(key),
        Err(e) => {
            console_error!("failed to upload image to the bucket: {:?}", e);
            Err(())
        }
    }
}

struct ImageUploader {
    img: DynamicImage,
    hash: String,
    dest_fmt: ImageFormat,
    dest_bucket: SendBucket,
}

#[derive(Debug, Serialize)]
struct UploadedImage {
    scale: u32,
    name: String,
}

impl ImageUploader {
    async fn upload_original_image(&self) -> Result<UploadedImage, ()> {
        let mut img_data = Vec::new();
        encode_image(&self.img, self.dest_fmt, &mut img_data)?;

        let name = upload_image_to_bucket(
            &self.hash,
            img_data,
            self.dest_fmt,
            self.dest_bucket.clone(),
        )
        .await?;
        console_log!("uploaded original image (name: {})", &name);

        Ok(UploadedImage { scale: 1, name })
    }

    async fn upload_upscaled_image(&self, scale: u32) -> Result<UploadedImage, ()> {
        let (w, h) = self.img.dimensions();
        let img = self.img.resize(w * scale, h * scale, FilterType::Nearest);

        let mut img_data = Vec::new();
        encode_image(&img, self.dest_fmt, &mut img_data)?;

        // stem (file name without extension) is the hash followed by the scale
        let stem = format!("{}_{}x", self.hash, scale);

        let name = upload_image_to_bucket(&stem, img_data, self.dest_fmt, self.dest_bucket.clone())
            .await?;
        console_log!("uploaded {}x upscaled image (name: {})", scale, &name);

        Ok(UploadedImage { scale, name })
    }
}
