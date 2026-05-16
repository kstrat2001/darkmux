# Photo Gallery

## Access for Family

The photo gallery is live at: **https://kstrat2001.github.io/darkmux/gallery/**

You can:
- View all photos in a beautiful grid layout
- Click any photo to see it full-size (lightbox view)
- Use arrow keys or buttons to navigate between photos
- Works on phones, tablets, and computers

## How Photos Are Added

Photos are stored in the `gallery/images/` folder. When new photos are added and pushed to GitHub, they automatically appear on the website within a few minutes.

### Adding New Photos

1. **Copy your photo** into the `docs/gallery/images/` folder
2. **Open** `docs/gallery/index.html` in a text editor
3. **Add the filename** to the images list:

```javascript
const images = [
  'IMG_001.jpg',   // <-- Add your photo filename here
  'sunset-beach.png',
];
```

4. **Commit and push** the changes to GitHub

That's it! The gallery will update automatically within 2-3 minutes.

## Supported Formats

- `.jpg` / `.jpeg`
- `.png`
- `.webp`

## Tips

- Use descriptive filenames like `sunset-beach.jpg`, `mountain-hike.png`
- For ordering, use numbers: `01.jpg`, `02.jpg`, etc.
- Photos are shown in the exact order you list them

---

**Questions?** Contact Kain for help adding photos!
