# Trip Photos

Add your photos here! The gallery will automatically display them.

## How to add a photo

1. Copy your image file into this folder
2. Edit `gallery/index.html` and add the filename to the `images` array:

```javascript
const images = [
  'IMG_001.jpg',   // <-- add your filename here
  'IMG_002.png',
];
```

## Tips

- Supported formats: `.jpg`, `.jpeg`, `.png`, `.webp`
- Files are shown in the order listed in `index.html`
- Use descriptive filenames like `sunset-beach.jpg`, `mountain-hike.png`
- For easy additions during the trip, use sequential numbering: `01.jpg`, `02.jpg`, etc.

## Example

If you take a photo and save it as `valley-view.jpg`:

1. Copy `valley-view.jpg` into this folder
2. Open `gallery/index.html`
3. Add `'valley-view.jpg'` to the images array

That's it!
